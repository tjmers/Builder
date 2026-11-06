# Audit of `tcp-listener-optimization`

Scope: `main...HEAD` plus the uncommitted working tree changes. No `CLAUDE.md` applies to this repo.

These findings come from reading the code and were not reproduced, as this review skips verification. The one exception is the Linux clippy result in finding 5, which comes from a real run. The Windows failure in that finding is reasoned from the code, because the Windows target isn't installed here. Findings are ranked most severe first.

## 1. Unread client data at close discards the whole response

- **Where:** `src/tcp_listener/linux.rs:71`
- **Problem:** `MSG_MORE` holds back the last partial segment until `close`. If any client bytes are still unread at that point, Linux resets the connection and throws away unsent data, so the held-back response is never sent.
- **Scenario:** A client writes the head and then the body of a `Transfer-Encoding: gzip` request. Framing completes on the head, and the body arrives before `close`. The same happens with a malformed chunked body that is still arriving, a CRLF sent after a POST body, or a pipelined request in a second write. A response smaller than one segment is lost entirely, so the client gets `ECONNRESET` and no 501, 400 or 200.
- **Compared with base:** Base sent without `MSG_MORE`, so the response had usually already left before the reset.
- **Suggested fix:** Drop `MSG_MORE`. If the FIN saving matters, keep it only when the receive queue is empty.

## 2. The idle sweep drops requests that are only waiting for a worker

- **Where:** `src/tcp_listener/linux.rs:323`
- **Problem:** `is_idle` only looks at `last_active`, so a parked connection whose request has already arrived counts as idle when no parked worker is free. The rescue thread also moves the kernel's backlog into this map, so those connections start the 5s timer instead of waiting in the queue.
- **Scenario:** On 4 CPUs, 30 clients call a 3s handler at once. At most 8 run at a time, and fewer when pinned workers are idle on their own sockets. Connections still waiting after 5 to 6s are closed with no response. Base's one thread per connection served all of them in about 3s.
- **Note:** `CHANGES_tcp-listener-optimization.md` lists this as a trade-off, but it silently loses requests under ordinary overload.
- **Suggested fix:** Before closing, check whether the socket has data with `MSG_PEEK` or `poll`, and skip connections that are ready.

## 3. Windows may cut off a client that is still downloading

- **Where:** `src/tcp_listener/windows.rs:190`
- **Problem:** `start_send` posts all the remaining bytes as one `WSASend`, and `touch` only runs when it completes. Bytes the client takes in the meantime don't count as progress, so after 5s `close_idle` calls `CancelIoEx` and `sent` closes the connection.
- **Scenario:** A 64MB response goes to a 1MB/s client over a real network. If the send stays pending past 5s, the download is truncated even though the client never paused.
- **Note:** `audit_mc0.md` saw complete transfers over loopback and marked real network paths as still open. This is why the risk remains.
- **Suggested fix:** Limit each `WSASend` to something like 64KB so completions show progress, and use the same 30s send timeout as Linux.

## 4. A non-UTF-8 header corrupts the body

- **Where:** `src/server.rs:194`
- **Problem:** When the head isn't valid UTF-8, the fallback converts the whole request with `from_utf8_lossy`, including the body. Each invalid byte can grow to 3 bytes, but `Content-Length` and the chunk sizes still count the original bytes.
- **Scenario:** A request has the Latin-1 header `X-Name: caf\xe9`, `Content-Length: 4` and the body `ff 00 fe 80`. `body()` returns `ef bf bd 00`, even though it is documented as returning the bytes as sent. With `Content-Length: 2`, the slice ends inside U+FFFD and the client gets a 400. Chunked bodies get a 400 too, or decode to the wrong bytes.
- **Suggested fix:** Convert only the head lossily, and take the body from the raw bytes after `head_len`.

## 5. The Windows clippy job fails on an unused import

- **Where:** `tests/tcp_roundtrip.rs:6`
- **Problem:** `Instant` is imported unconditionally, but its only uses (lines 286 and 341) are in `#[cfg(target_os = "linux")]` tests. CI runs `cargo clippy --workspace --all-targets -- -D warnings` on `windows-latest`, so `unused_imports` becomes an error.
- **Suggested fix:** Gate the import on Linux, or use `std::time::Instant` inside those two tests.
- **Also found:** The same clippy command already fails on Linux at `benches/compare.rs:26` (`type_complexity`, with cargo 1.98). That code comes from `main`, not this branch, and nothing else fails on Linux.

## 6. Fast close can lose responses up to 64KB on some clients

- **Where:** `src/tcp_listener/linux.rs:116`
- **Problem:** With `TCP_LINGER2` at -1, the client's ACK of the FIN is answered with a reset. The code's own comment says a client that discards unread data on a reset could lose the end of the response. The 64KB cutoff doesn't remove that risk, because a slow reader can lose a smaller response too.
- **Scenario:** A client or proxy that discards its receive buffer on a reset hasn't finished reading a 10KB response when the reset arrives. It gets a truncated response or `ECONNRESET`.
- **Note:** `audit_mc0.md` didn't reproduce this with one Windows build over the WSL network. Other clients and proxies are untested.
- **Suggested fix:** Test real clients and proxies over a real network path, or accept `TIME_WAIT` and use the normal close.

## 7. Framing and the parser disagree about valid and invalid requests

- **Where:** `src/tcp_listener.rs:105`
- **Problem:** `request_len` reads headers separately from `Head::parse`, and `header_end` and `head_len` separately find the end of the head, so they can disagree.
- **Scenarios:**
  - **Regression:** `Transfer-Encoding: chunked` with `Content-Length: 5` and the body `hello` is framed as chunked and stays `Incomplete`. The client waits 5 to 6s and is closed with no response, where base answered 400 straight away. This is the case the parser rejects as a likely smuggling attempt.
  - **Also on base:** `\r\n\r\nGET / HTTP/1.1...` frames as a 4-byte request and gets a 400, although the parser is written to ignore leading blank lines.
  - **Also on base:** A head ending in `\n\r\n`, which `next_line` accepts, is never seen as complete and is closed after the idle timeout.
- **Suggested fix:** Have framing and the parser share one head scan, so that they can't disagree, as `walk_chunks` already does for chunks.

## 8. Chunked framing lets one client burn a lot of CPU

- **Where:** `src/tcp_listener.rs:71`
- **Problem:** `walk_chunks` restarts from the first chunk after every `recv`, so the total work grows with the square of the body size.
- **Scenario:** A client sends 1MB of `1\r\na\r\n` (about 175,000 chunks) in tiny writes, and any progress resets the idle timer. Assuming a few milliseconds per walk, one connection can use minutes of CPU, and a handful of them can occupy all the parked workers. `CHANGES_tcp-listener-optimization.md` says the 1MB limit bounds each walk, but it doesn't bound the number of walks.
- **Suggested fix:** Store the offset of the last complete chunk on the connection and resume from there.

## 9. A flood on one socket can stop the rescue thread doing anything else

- **Where:** `src/tcp_listener/linux.rs:688`
- **Problem:** `take_over` loops until `accept` returns `EWOULDBLOCK`, and the same thread also handles the idle sweep and every other worker's socket.
- **Scenario:** While a worker is stalled, connections arrive on its socket at least as fast as the rescue thread can accept and park them. `take_over` doesn't return, so idle connections are never closed and other stalled workers aren't rescued. The parked map also keeps growing until the process hits `EMFILE`.
- **Suggested fix:** Cap the accepts per tick, for example at 64, and stop taking over when too many connections are already parked.

## 10. The rescue thread polls constantly, even on an idle server

- **Where:** `src/tcp_listener/linux.rs:651`
- **Problem:** The thread wakes 100 times a second, allocates a new `Vec<pollfd>`, and polls every listening socket, even when there is no traffic. Connections on a stalled worker's socket also wait up to about 20ms. The cause is handlers running inline on the pinned accept threads.
- **Cost:** This means constant idle wake-ups on every deployment, and a 10 to 20ms delay for any connection that lands behind a slow handler.
- **Suggested fix:** Keep the `Vec` in `Rescue`, and only poll while a worker is inside a handler. A deeper fix is to pass work to the parked pool once a handler runs too long, which could remove the polling. `audit_mc0.md` also raised whether the rescue thread is still needed.
