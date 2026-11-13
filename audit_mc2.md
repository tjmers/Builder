# Audit of `tcp-listener-optimization` (mc2)

Scope: `origin/tcp-listener-optimization...HEAD` plus the uncommitted working tree changes, which are the fixes for `audit_mc1.md`. No `CLAUDE.md` applies to this repo.

Findings 1 to 5 were reproduced on loopback with a throwaway probe program built outside the repo. Findings 6 to 10 come from reading the code and were not reproduced, and nothing was run on Windows. `cargo clippy --workspace --all-targets -- -D warnings` is clean on Linux. Findings are ranked most severe first.

## 1. The send timeout closes clients that are reading slowly but steadily

- **Where:** `src/tcp_listener/linux.rs:322`
- **Problem:** The 30s send timeout measures the time since the server's last successful `send`, not the client's progress. A parked connection is only driven again on `EPOLLOUT`, which Linux reports once about a third of the send buffer has drained, so a slow reader looks idle for the whole of that time.
- **Scenario:** A client reads a 64MB response at a steady 16KB/s. It is closed at 30s with the log line "Closing connection that stopped reading after 2625600 of 67108969 bytes" (the send buffer is 2.6MB). Any rate-limited download slower than about a third of the send buffer per 30s gets a truncated body.
- **Note:** The existing test only covers a 7s pause.
- **Suggested fix:** Measure progress by what the client has taken, for example by comparing the unsent bytes in the socket (`SIOCOUTQ`) between sweeps, and close only when that hasn't moved for 30s.

## 2. Unread client data at close still discards the end of a large response

- **Where:** `src/tcp_listener/linux.rs:416`
- **Problem:** The connection is closed as soon as the last byte is in the kernel's send buffer. If any client bytes are unread at that point, the close becomes a reset, which discards whatever part of the response hasn't been sent yet.
- **Scenario:** A client reads an 8MB response at a limited rate. With nothing extra sent, all 8,388,712 bytes arrive. With a 2-byte CRLF sent after the request in a later segment, or 64KB of trailing bytes, the client gets 6,514,368 bytes and `ECONNRESET`. A pipelined request, a CRLF after a POST body, or bytes past `Content-Length` all trigger it.
- **Note:** The comment at line 73 and `CHANGES_tcp-listener-optimization.md` say the response "has been sent by then". That only holds for responses that fit in the first flight, and the new test uses a 5-byte body. This is the remaining part of finding 1 in `audit_mc1.md`.
- **Suggested fix:** Before closing, call `shutdown(SHUT_WR)` and drain or discard the client's unread bytes until the send buffer is empty, then close.

## 3. 512 idle connections switch the rescue thread off

- **Where:** `src/tcp_listener/linux.rs:715`
- **Problem:** `MAX_PARKED` counts every parked connection, including idle and slow ones, so 512 cheap idle sockets stop the rescue thread taking anything over.
- **Scenario:** On a server pinned to one CPU, 520 sockets each send a partial request line. A fast request behind a 1s handler then takes 900ms, against 3.5ms with none parked. A client can hold those sockets open indefinitely by sending a byte every few seconds, because any byte resets the idle timer.
- **Note:** `CHANGES_tcp-listener-optimization.md` lists the cap as a trade-off, but not that idle connections count toward it.
- **Suggested fix:** Count only rescued connections that are still waiting for a worker.

## 4. Rescued connections wait for a parked worker while the pinned worker sits idle

- **Where:** `src/tcp_listener/linux.rs:722`
- **Problem:** `take_over` moves up to 64 queued connections into the parked map whether or not a parked worker is free. Once they are there, the pinned worker can no longer reach them.
- **Scenario:** 14 one-second requests arrive on a server pinned to one CPU, with 1 pinned and 4 parked workers. They finish in batches of 5, 4, 4 and 1 over 4s instead of 3s. The pinned worker serves one request, returns to an empty accept queue and sits idle while 9 ready connections wait for a parked worker. New arrivals on that socket are also served ahead of older rescued ones.
- **Suggested fix:** Take over only as many connections as there are free parked workers, and leave the rest in the kernel's queue for the pinned worker.

## 5. Chunked framing is still quadratic for an unterminated line

- **Where:** `src/http_request.rs:290`
- **Problem:** `ChunkWalker` only keeps its place between whole chunks. An unterminated chunk-size, extension or trailer line is rescanned from its start on every receive.
- **Scenario:** After a chunked head, a client sends 1MB of `1` with no newline in 64-byte writes. That costs 3.3s of CPU, against 0.8 to 0.9s for the same traffic as a `Content-Length` body or as tiny chunks. Smaller writes grow the cost with the square of the size, up to the 1MB limit.
- **Note:** This is the case finding 8 of `audit_mc1.md` targeted. The new test only covers many complete tiny chunks.
- **Suggested fix:** Store how far the current line has been scanned and resume from there, or reject a size or trailer line longer than a small limit.

## 6. Windows cuts off clients slower than about 2.2KB/s

- **Where:** `src/tcp_listener/windows.rs:46`
- **Problem:** Send progress is only seen when a whole 64KB `WSASend` completes, so a pending send shows no progress until the client has taken all of it.
- **Scenario:** A client takes a large response at 1KB/s and needs 64s for each 64KB piece. The pending send shows no progress for 30s, `close_idle` calls `CancelIoEx`, and `sent` closes the connection as "stopped reading".
- **Note:** This has the same root cause as finding 1, with a much lower threshold.
- **Suggested fix:** Send smaller pieces, or raise the timeout to match the slowest client the piece size should allow.

## 7. The idle sweep logs and closes while holding the parked lock

- **Where:** `src/tcp_listener/linux.rs:518`
- **Problem:** `close_idle` logs, polls and closes inside the `retain` closure with the parked mutex held, so a slow or blocked stdout stalls every thread that parks or takes a connection.
- **Scenario:** Logging to stdout is on and stdout is a full pipe. `println!` blocks with the lock held, and all parked workers, plus any pinned worker that needs to park, wait on the mutex.
- **Suggested fix:** Collect the fds to close under the lock, then log and close after releasing it.

## 8. An empty raw response is logged as an error on Windows only

- **Where:** `src/tcp_listener/windows.rs:449`
- **Problem:** An empty response makes `start_send` issue a 0-byte `WSASend`, which completes with 0 and hits the `Ok(0)` arm. Linux's `send_response` loop never sends and returns `Ok(true)`.
- **Scenario:** A `RawRequestHandler` returns `Cow::Borrowed(b"")`, as the test handlers do. Windows logs "send: no bytes were sent", and Linux closes silently.
- **Suggested fix:** Return `Next::Close` from `received` when the response is empty.

## 9. `SO_INCOMING_CPU` is defined by hand, and steering is skipped on some architectures

- **Where:** `src/tcp_listener/linux.rs:424`
- **Problem:** The constant is hand-defined behind an architecture list with a no-op fallback, and the comment says the libc crate doesn't export it. The locked libc (0.2.189) exports `libc::SO_INCOMING_CPU` for every Linux target.
- **Cost:** On unlisted architectures (powerpc64, s390x, mips, loongarch64), steering is silently skipped, although libc has the right value, including sparc's different one. The duplicate constant and the two `cfg` variants of `steer_to_cpu` aren't needed.
- **Suggested fix:** Use `libc::SO_INCOMING_CPU` and drop the `cfg`s.

## 10. `Drop for Shared` can never run

- **Where:** `src/tcp_listener/linux.rs:529`
- **Problem:** `Rescue::run` never returns, so the thread scope in `run` doesn't either, and the `Drop` that closes the parked fds and the epoll fd is dead code. The per-worker sockets from `open_listener` have no owner that closes them at all.
- **Cost:** The cleanup reads as if `run` can return, which it can't.
- **Suggested fix:** Delete the `Drop` impl, or give `run` a real shutdown path that also closes the worker sockets.
