# Audit of `tcp-listener-optimization` (mc3)

Scope: `main...HEAD` plus the uncommitted working tree changes, which are the fixes for `audit_mc2.md`. No `CLAUDE.md` applies to this repo.

These findings come from reading the code and were not reproduced, as this review skips verification, and nothing was run on Windows. Two things were run: `cargo clippy --all-targets` is clean on Linux, and the three edited `.vsdx` files are still valid zip archives with well-formed XML. Findings 1 to 4 are correctness problems and 5 to 10 are cleanup. They are ranked most severe first.

## 1. Windows still resets the connection when client data is unread at close

- **Where:** `src/tcp_listener/windows.rs:465`
- **Problem:** `sent` returns `Next::Close` as soon as the last `WSASend` completes, and `finish` calls `closesocket` straight away. A send completes once the data is in the socket's buffer, not once the client has it. If the client has sent anything the server never read, Windows closes with a reset and discards whatever is still buffered. This is finding 2 of `audit_mc2.md`, which was fixed on Linux only (`Connection::finish` and `drain`).
- **Scenario:** A client sends a request, then a CRLF or a pipelined request in a later segment, and reads a 1MB response slowly. The server buffers the last 16KB piece, closes, and the client gets a truncated body and `WSAECONNRESET`. A small response can be lost whole if it has not left the buffer yet.
- **Note:** `CHANGES_tcp-listener-optimization.md` lists the reset trade-off for Linux only, and the two tests that cover it are gated to Linux.
- **Suggested fix:** Do the same lingering close on Windows: call `shutdown(SD_SEND)`, keep receiving and discarding until the client closes or the send timeout passes, then close. `shutdown` needs `SO_UPDATE_ACCEPT_CONTEXT` set after `AcceptEx`, which `post_accept` currently skips on purpose.

## 2. A client that keeps sending is never timed out while its response lingers

- **Where:** `src/tcp_listener/linux.rs:409`
- **Problem:** `expired` never closes a connection whose socket is ready, and that exemption also applies to a `closing` connection. Its socket is ready whenever the client has sent more data. The comment on `drain` says such a client "is still closed by the sweep", but the sweep can only close it at a moment when it is parked and has nothing waiting to be read.
- **Scenario:** A client requests a response too large to go out at once, reads none of it, and streams junk continuously. Each `drain` pass discards up to 64KB and parks the connection, which is ready again at once. The sweep finds it either out of the map or ready, so it survives well past the 30s send timeout, holding its file descriptor and response and keeping a parked worker busy. Over loopback or a fast link this can last as long as the client keeps sending. Before this change the connection was closed right after the last `send`.
- **Suggested fix:** Skip the readiness exemption when `closing` is set (`self.is_idle() && (self.closing || !is_ready(..))`), or cap the total bytes a connection may have discarded.

## 3. A failed log write kills workers for good and leaves the server holding the port

- **Where:** `src/tcp_listener/linux.rs:471`
- **Problem:** `log_info!` and `log_error!` use `println!`, which panics when stdout fails. Only the handler call is inside `catch_unwind`. A panic anywhere else in a worker ends that thread permanently, and because `run` never returns, the thread scope never reports it.
- **Scenario:** Logging to stdout is on and stdout is a pipe whose reader has exited. Each pinned worker panics in `accept` on its next connection, leaking the accepted socket. The rescue thread then panics the same way inside `take_over`, with the listening socket left non-blocking, and the scope waits forever for the parked workers. The process stays alive with the port open and never answers another connection. On `main` the same panic was in the calling thread's accept loop, so the server exited.
- **Note:** The same applies on Windows (`windows.rs:389`). A failed `scope.spawn` at startup has the same effect.
- **Suggested fix:** Make the logger write with `writeln!` and ignore the error, and treat a panic outside a handler as fatal (abort the process) instead of losing the thread.

## 4. "All gone out" does not mean the client has the response

- **Where:** `src/tcp_listener/linux.rs:355`
- **Problem:** `finish` closes at once when `SIOCOUTQNSD` reports nothing unsent, on the grounds that such a response "is safe" from a reset. Sent is not delivered. A close with unread client data makes the kernel drop the connection without retransmitting, so a response segment lost on the way is never resent. A client that discards received data on a reset loses it too, which is the case `CHANGES_tcp-listener-optimization.md` already lists.
- **Scenario:** A client sends a POST followed by a CRLF, or a body after a head that is answered early (the 400 for `Transfer-Encoding` with `Content-Length`, a 501 for another coding, a malformed chunk). The response fits in one flight, so the connection is closed and reset. On a network that drops that segment, the client gets `ECONNRESET` and no response.
- **Note:** This is the remaining part of finding 2 in `audit_mc2.md`. The documented trade-off covers only clients that discard on reset, not loss in transit.
- **Suggested fix:** Take the lingering path whenever client data is unread at close (one non-blocking `recv` or `FIONREAD` shows it), not only when something is unsent.

## 5. A lingering connection keeps the whole response in memory

- **Where:** `src/tcp_listener/linux.rs:358`
- **Problem:** Once `send_response` returns true, every byte is in the kernel's send buffer, but the connection keeps its `Cow` until it is closed. `expired` and the sweep's log only need the response's length.
- **Cost:** Each slow client finishing a large owned response holds that buffer (64MB in the tests) while only the last few MB in the kernel still matter. This lasts until the client closes, or up to 30s after it stops acknowledging.
- **Suggested fix:** In `finish`, replace the response with its length before parking.

## 6. Any request that needs a second `recv` doubles its buffer and loses buffer reuse

- **Where:** `src/tcp_listener/linux.rs:297`
- **Problem:** `recv_request` calls `reserve(RECV_BYTES)` before every `recv`. After a first `recv` that returned anything, the 16KB buffer has less than 16KB spare, so `reserve` reallocates it to 32KB and copies. The buffer is then over `RECV_BYTES`, so it is dropped instead of recycled. `start_recv` in `windows.rs:181` does the same.
- **Cost:** A request whose head and body arrive in separate segments pays a reallocation, a copy and a fresh allocation for the next connection, even when the whole request is a few hundred bytes. This includes the `recv` that only returns `WouldBlock`. The benchmark sends each request in one write, so it doesn't show this.
- **Suggested fix:** Reserve only when the spare room is small (under a few KB, say), or recycle buffers up to `2 * RECV_BYTES`.

## 7. The head is walked four times, twice by duplicate code

- **Where:** `src/http_request.rs:145`
- **Problem:** `head_len` repeats the line walk in `scan_head`. The comment in `tcp_listener.rs` says framing uses "the parser's own scan (`scan_head`), so the two cannot disagree", but `Server::handle_raw` splits the request with `head_len`, a second copy that has to be kept in step by hand.
- **Cost:** Each request's head is read by `scan_head` (framing), `head_len` (`split_head`), `to_text` (UTF-8 check) and `Head::parse`. Disagreement between two head scans was finding 7 of `audit_mc1.md`.
- **Suggested fix:** Define `head_len` as `scan_head(request).map(|(len, _)| len)`, or build both on one shared line walk.

## 8. Both listeners carry the same handler block and `RECV_BYTES`

- **Where:** `src/tcp_listener/linux.rs:486`
- **Problem:** Lines 486 to 501 of `linux.rs` and 424 to 439 of `windows.rs` are the same code: truncate to the framed length, call the handler under `catch_unwind`, log a panic, and reset or drop the request buffer. `RECV_BYTES` and its comment are also defined in both files, while `SEND_IDLE_TIMEOUT` was moved to the shared module for this reason.
- **Cost:** A change to panic handling or buffer recycling has to be made twice, and the Windows copy can't be tested on this machine.
- **Suggested fix:** Move the block into one function in `src/tcp_listener.rs`, along with `RECV_BYTES`.

## 9. The new tests copy existing helpers

- **Where:** `tests/tcp_roundtrip.rs:307`
- **Problem:** The `unsafe` block that pins the server thread to one CPU appears three times (lines 307, 545 and 670). `large_response_survives_a_client_pause` builds its own 64MB server and body-length check, although `start_large_server` and `body_len` now do both.
- **Cost:** Three copies of an `unsafe` block, and about 15 duplicated lines in a file that grew by 660 lines.
- **Suggested fix:** Give `start_pinned_server` the endpoints as a parameter, or split out a `pin_to_current_cpu` helper, and use `start_large_server` and `body_len` in the older test.

## 10. The uncommitted `scripts/` stubs don't match the crate

- **Where:** `scripts/build.rs:9`
- **Problem:** `build.rs` calls `builder::compile_views`, which doesn't exist in the crate. Its comment says it "should live in src/build.rs", but Cargo looks for a build script at the package root, and a build script can only use `builder` as a build dependency. `builder.sh` is comments only, with an unfinished sentence on line 4, and `build.sh` and `views_mod.rs` contain no code.
- **Cost:** Nothing is broken, since none of these files are built or run. They would fail as soon as they were used, so they shouldn't be committed with this branch as they are.
- **Suggested fix:** Leave them out of this branch, as `CHANGES_tcp-listener-optimization.md` already says they are.
