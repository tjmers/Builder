# Changes on `tcp-listener-optimization`

This branch replaces the server's one-thread-per-connection model with a fixed pool of worker threads that use each OS's native event system: epoll on Linux and I/O completion ports on Windows. It also makes HTTP request parsing cheaper by checking the request body only when a handler reads it.

| | |
|---|---|
| **Compared against** | `3a72253`, where the branch split from `main` |
| **Commits** | `e0f62d7` (Linux and parsing), `16445a3` (Windows) |
| **Size** | 8 files, +1581 / -307 lines, before the audit fixes |
| **Audit fixes** | The fixes for `audit.md`, committed in `39ec2b7` |
| **Second audit fixes** | The fixes for all 10 findings in `audit_mc1.md`, committed in `46c9b96` |
| **Third audit fixes** | The fixes for all 10 findings in `audit_mc2.md`, committed in `e2f6992` |
| **Fourth audit fixes** | The fixes for findings 1 to 9 of `audit_mc3.md`, not committed yet. Finding 10 is about files that aren't part of the branch (see [Not included](#not-included)) |

## Before and after

| | Before | Linux now | Windows now |
|---|---|---|---|
| **Threads** | One new thread per connection | One pinned worker per CPU, `max(CPUs, 4)` parked workers, plus a rescue and sweep thread | Two workers per core, plus a pool and sweep thread |
| **Socket I/O** | Blocking `recv` and `send` | Non-blocking, with epoll for connections that have to wait | Overlapped `WSARecv` and `WSASend`, with a completion port |
| **Accepting** | One blocking `accept` loop | Each worker has its own `SO_REUSEPORT` socket and waits in a blocking `accept4` | A pool of 64 outstanding `AcceptEx` calls |
| **Idle timeout** | 5s per `recv` (`SO_RCVTIMEO`) | Swept every second: 5s without receiving anything, or 30s without the client acknowledging any of the response | Swept every second, using `CancelIoEx`: 5s while receiving, 30s for each 16KB piece of the response |
| **Handler panic** | Not caught | Caught; only that connection is lost | Caught; only that connection is lost |
| **Closing** | `close` right after the last `send` | `close` right away unless some of the response is unsent or client data is unread; then it waits for the client to take the response | `closesocket` right away unless client data is unread; then it waits for the client to close |

An idle connection, such as a browser's unused preconnect socket, now takes up a map entry instead of a thread.

## Linux listener (`src/tcp_listener/linux.rs`)

### Threads
- **Pinned workers:** one per CPU the calling thread may use, each pinned to that CPU with its own listening socket. `SO_INCOMING_CPU` steers connections to the worker on the CPU that handled their packets. The usual path is `accept4`, `recv`, `send`, `close`, with no epoll calls and no shared lock. In a counted benchmark run with `SCHED_IDLE` still on, 32 of 5.4 million connections left this path.
- **Parked workers:** `max(CPUs, 4)` unpinned threads waiting in `epoll_wait` on one shared epoll, taking one event at a time. Taking a batch would leave the rest waiting behind that thread's handler.
- **Rescue thread:** the calling thread checks every 10ms for a worker that has made no progress while connections wait on its socket, and moves as many of those connections to the parked workers as there are parked workers free. It never runs a handler, so a slow handler can't stop it. It also closes idle connections once a second. After 10 checks in a row that find nothing going on, it stops checking and waits in `poll` on the workers' sockets until a connection arrives or the sweep is due, so an idle server wakes about twice a second instead of 100 times.

Workers run under the default scheduling policy. `SCHED_IDLE` starved handlers on a busy CPU, and `SCHED_BATCH` made fast requests about 50 times slower there (see [Performance](#performance)).

### Parking a connection
- A connection that can't finish without blocking is put in a map behind a mutex and registered with `EPOLLONESHOT`. It goes in the map first, because its event can fire before `epoll_ctl` returns.
- A parked worker takes the connection out of the map while driving it, so only one thread holds it at a time and the sweep can't close it.
- The lock is held across the map insert and `epoll_ctl`, so the sweep can't close the fd in between.
- An event with no map entry is ignored. A stale event that finds a new connection on a reused fd is harmless, because that worker finds nothing to read and parks it again.
- Requests are received straight into the request buffer, with no intermediate chunk copy or zeroing. Each worker also keeps one finished connection's buffer to reuse.
- The buffer grows only when less than 4KB of it is spare. Before the fourth audit fixes it was grown before every `recv`, so any request that needed a second `recv` had its 16KB buffer reallocated to 32KB, which was then too large to reuse.

### Taking over from a stalled worker
The rescue thread makes the socket non-blocking only while it takes connections from the queue, so it can't get stuck in `accept` if the worker wakes and takes the connections first. A worker already blocked in `accept` stays blocked, and one that arrives during that window gets `EAGAIN` and tries again.

Each check, it takes only as many connections as there are parked workers waiting in `epoll_wait`, and at most 64 from one socket. Each parked worker counts itself in `free_workers` while it waits. The rest stay in the kernel's queue, where they hold no file descriptor and the stalled worker can still take them when it comes back. Before the third audit fixes it took up to 64 whether or not a worker was free, so a pinned worker could come back to an empty queue while its connections waited for a parked worker. It also stopped taking anything while 512 connections were parked, which idle connections counted toward. That limit is gone: only the number of free workers limits it now.

### Sending and closing
- **Send timeout:** a client is closed when it has acknowledged none of its response for 30s. Each sweep asks the socket how many sent bytes are unacknowledged (`SIOCOUTQ`) and treats any fall as progress. Before the third audit fixes progress was a successful `send`, which a slow client allows only once about a third of the send buffer has drained, so a steady download slower than that was cut off.
- **Closing:** closing a connection while client data is unread (a pipelined request, a line ending after the request) resets it. A reset discards whatever part of the response hasn't gone out, and stops the kernel resending anything lost on the way. After the last `send`, the worker asks how much is still unsent (`SIOCOUTQNSD`) and how much the client has sent that is unread (`FIONREAD`). If both are zero, it closes straight away. Otherwise it shuts down its sending side, discards what the client sends, and parks the connection until the client closes or has acknowledged everything. Before the fourth audit fixes only unsent data was checked, so a response that had all gone out was still closed with a reset.
- **Lingering connections:** a connection waiting this way drops its copy of the response, since the kernel has its own, and keeps only the length. Nothing the client sends counts as progress. The 30s send timeout is checked each time its data is discarded as well as by the sweep, and the sweep no longer skips a lingering connection because its socket is ready. Before the fourth audit fixes, a client that kept sending was ready at every sweep, so it was never timed out.
- **The sweep** removes expired connections from the map with the lock held, then logs and closes them after releasing it, so a blocked log can't hold up the workers.

### Running out of file descriptors
When `accept4` fails with `EMFILE`, `ENFILE`, `ENOBUFS` or `ENOMEM`, the worker sleeps for 100ms before trying again and logs once per run of failures. The rescue thread pauses for the same 100ms.

### Socket options

| Option | Effect |
|---|---|
| `SO_REUSEPORT` | Gives each pinned worker its own listening socket on the same port |
| `SO_INCOMING_CPU` | Sends a connection to the socket of the worker on the CPU that handled its packets |
| `TCP_DEFER_ACCEPT` (1s) | Holds a connection back from `accept` until its first data arrives, so the worker is woken only once |
| `SOCK_CLOEXEC` | Stops sockets being inherited by child processes |

`SO_INCOMING_CPU` comes from the `libc` crate, so steering applies on every architecture. Before the third audit fixes it was defined by hand for five architectures and skipped on the rest.

`TCP_LINGER2` (-1) and `MSG_MORE` were removed by the second audit fixes. `MSG_MORE` held the end of the response back until `close`, and a close with client data still unread resets the connection and discards it, so the client got no response at all. `TCP_LINGER2` at -1 answered the client's final ACK with a reset, which could cost a client that discards unread data on a reset part of a response it had already received. Connections now close normally and pass through `TIME_WAIT`.

### Binding
The first socket binds and listens without `SO_REUSEPORT`, and turns it on only after `listen` succeeds. The workers' other sockets then join it.
- **Two servers:** both can bind with `SO_REUSEADDR` before either listens, but the second `listen` fails with `EADDRINUSE`, so its `run` returns false. A server that binds after the first is listening fails in `bind`.
- **Port 0:** the port the OS assigned to the first socket is read back and used for the rest, so all workers share one port.
- **Failed `bind`:** also closes its socket.

## Windows listener (`src/tcp_listener/windows.rs`)

- Worker threads take completions from a single completion port, which runs at most one worker per core at a time. The extra workers can take over when a handler blocks.
- Each connection is a boxed struct that the kernel owns while an operation is outstanding, and it comes back through the completion.
- When a call finishes immediately, its completion is skipped (`FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`) and the same thread carries on. This is turned off if a layered socket provider without real file handles is installed, because it could hand one connection to two threads at once.
- The calling thread tops up the `AcceptEx` pool and sweeps for idle connections once a second.
- Last-activity times are kept in one `Mutex<HashMap>` shared by all workers, with a flag for whether the connection has a response to send.
- **Closing:** after the last `WSASend` completes, the worker checks for unread client data (`FIONREAD`). If there is none it closes straight away. Otherwise it calls `shutdown(SD_SEND)` and keeps receiving and discarding (a new `Drain` operation) until the client closes, a receive fails, or 30s have passed since the shutdown. `shutdown` needs `SO_UPDATE_ACCEPT_CONTEXT` on a socket from `AcceptEx`, which is set only on this path. Before the fourth audit fixes every connection was closed right after the last send, which reset the connection and could discard a response still in the socket's buffer.
- A response is sent in pieces of at most 16KB (64KB before the third audit fixes). A `WSASend` only completes once the client has taken all of it, so one send of a whole large response showed no progress until the end, and a client downloading it slowly looked the same as one that had stopped reading. A piece is the least progress the listener can see, so a client slower than one piece per 30s, about 550 bytes a second, is still closed.
- An empty response closes the connection without a send. Before, the 0-byte send was logged as an error.
- `Cargo.toml` adds three `windows-sys` features: `Win32_Foundation`, `Win32_Storage_FileSystem` and `Win32_System_IO`.

## Shared listener code (`src/tcp_listener.rs`)

- **Added:** `Framing` and `Framer`, which apply the same size limits and request framing on both platforms, and `log_closed_early`. Each connection has a `Framer`, which keeps its place between receives: the head is read once, a `Content-Length` body costs one comparison per receive, and a chunked body is walked once however many pieces it arrives in.
- **One head scan:** `Framer` reads the head with the parser's own `scan_head` (see below), so the two agree on where the head ends and what kind of body follows. Headers the parser will reject, such as `Transfer-Encoding` with `Content-Length`, are handed to it straight away for a `400`.
- **Header limit:** a head over 8KB is now refused even when it arrives complete in one receive.
- **Chunked framing:** chunked bodies are framed when the first `Transfer-Encoding` header is `chunked`. Malformed chunks are handed to the parser, which answers `400`. The 1MB limit counts the body with its chunk framing.
- **Removed:** `recv_chunk`, `recv_request`, `send_all`, `serve`, `serve_connection`, `RECV_CHUNK_BYTES`, the `platform` alias, and `header_end`, which `scan_head` replaces.
- **Changed:** `IDLE_TIMEOUT` is now `pub(crate)`, and `SEND_IDLE_TIMEOUT` (30s) lives here so both platforms use it.
- **Added by the fourth audit fixes:**
  - `TcpListener::handle` runs the handler under `catch_unwind` and recycles the request buffer. Both listeners had their own copy of this.
  - `RECV_BYTES` and `reserve_recv_room`, also shared now.
  - `AbortOnPanic`, a guard held by every listener thread that aborts the process if the thread panics. Only a handler's panic is caught, so any other panic used to end its thread for good while `run` carried on, leaving a server that held the port and answered nothing.

## Logger (`src/logger.rs`)

`Logger::log` writes with `writeln!` and ignores a failed write. It used `println!`, which panics when stdout can't be written to, such as a pipe whose reader has exited.

## HTTP parsing (`src/http_request.rs`, `src/server.rs`)

- **`HttpRequest::split_head` and `from_parts` (new, `pub(crate)`):** the request is split into its head and what follows, and only the head is turned into text. The body is taken from the bytes as they were sent.
- **Lazy body text:** `content()` still returns `&str`, but converts the body on first call and caches it in a `OnceLock`.
- **`Server::handle_raw`:** now uses `split_head` and `from_parts`. If the head isn't valid UTF-8, its invalid bytes are replaced and the parser judges what is left. Only the head is converted, so the body and its lengths are untouched.
- **New `Head` struct:** collects `Host`, `Content-Length` and `Transfer-Encoding` during the single pass over the headers. This replaces `parse_body` and the later header re-scans.
- **`BodyHeaders` and `scan_head` (new):** `BodyHeaders` holds the rules for what kind of body a request has: the first `Transfer-Encoding` counts, repeated `Content-Length` headers must agree, and both together is invalid. `Head::parse` and `scan_head`, which the listener uses for framing, both feed their headers through it, and both split lines the same way.
- **`HttpRequest::body` (new, public):** returns the body's bytes as sent, with any chunked encoding removed.
- **`ChunkWalker` and `walk_chunks` (new, `pub(crate)`):** the chunk walk shared by framing and `decode_chunked`, so the two can't disagree about where a body ends. `ChunkWalker` can stop when it runs out of bytes and carry on from where it stopped when more arrive: inside a chunk's data, or partway along a size or trailer line. Before the third audit fixes it went back to the start of the chunk, so an unfinished line sent in small pieces was read again on every receive.
- **`walk_head` (new):** the one walk over the head's lines. `scan_head` and `head_len` (which `split_head` uses) both call it. Before the fourth audit fixes each had its own copy of the walk.
- **`decode_chunked`:** now works on bytes.
- **Smaller speedups:**
  - A `TCHAR` lookup table for validating tokens
  - Plain byte scans instead of `str::find`
  - Checking origin-form targets first
  - A quick check that lets most paths skip dot-segment handling

## Behavior changes

Anyone relying on the old behavior should check these:

1. **Bodies that aren't valid UTF-8:** `Content-Length` is now counted on the raw bytes, so a length that ends partway through a multi-byte character reaches the handler instead of returning `400`. `content()` replaces the invalid bytes, and `body()` returns them unchanged. `HttpRequest::new` still rejects this case.
2. **Chunked bodies:** these now work over a socket, where they always got a `400` before. Chunk sizes are counted on raw bytes, and a chunk boundary can fall inside a multi-byte character.
3. **Stalled sends:** a client that stops reading its response is now disconnected. Before, a blocking `send` had no timeout. The limit is 30s without progress on both platforms: any acknowledged byte on Linux, and a 16KB piece on Windows.
4. **Idle timing:** connections now close after 5–6 seconds instead of 5. On Linux, `TCP_DEFER_ACCEPT` can add about another second before a silent connection is accepted and its timer starts.
5. **Handler panics:** the connection is closed with no response, and the worker keeps running.
6. **Windows logging:** the `Connection from` line no longer includes the client's port.
7. **Large headers:** a head over 8KB is now dropped even when it arrives complete. Before, the limit only applied while the head was still incomplete.
8. **Headers that aren't valid UTF-8:** the body is no longer affected. Before, the whole request was converted, so each invalid byte in the body grew to 3 bytes while `Content-Length` and the chunk sizes still counted the original bytes.
9. **Head framing:** the listener now finds the end of the head the way the parser does. Empty lines before the request line are skipped, and a head whose lines end in a mix of LF and CRLF is complete. Before, the first got a `400` and the second was closed after the idle timeout.
10. **`Transfer-Encoding` with `Content-Length`:** answered with `400` as soon as the head arrives. Before the second audit fixes it was framed as chunked and closed after the idle timeout with no response.
11. **`TIME_WAIT` on Linux:** connections go through the normal close again, so each one stays in `TIME_WAIT` after the server closes it.
12. **Chunk data not followed by a line ending:** answered with `400` as soon as the next byte arrives. Before the third audit fixes the listener waited for a line ending first.
13. **Large responses on Linux:** the connection stays open after the last `send` until the client has the response, where it was closed at once before.
14. **Unread client data at close:** a client that sent something after its request (a line ending, a pipelined request) no longer gets a reset. On both platforms the connection stays open until the client has the response (Linux) or closes its end (Windows), for up to 30s.
15. **Log failures:** a log line that can't be written is dropped. Before, the `println!` panicked: on `main` that ended the server, and on this branch it ended the worker thread.
16. **Panics outside a handler:** a panic in a listener thread anywhere but a request handler now aborts the process. A failure to spawn a worker thread does the same.

The public `TcpListener` and `HttpRequest` methods keep their signatures, and `HttpRequest` gains `body()`.

## Trade-offs

- **Linux throughput:** see [Performance](#performance). Each request takes about 1.5 to 3 times as long as before the audit fixes on this benchmark, nearly all from dropping `SCHED_IDLE`.
- **Slow handlers on Linux:** a slow handler takes up one pinned worker, and new connections queued on its socket wait up to about 20ms for the rescue thread. If every parked worker is also in a slow handler, those connections wait for one to come free. The idle sweep leaves them alone, because it skips a connection whose socket is ready. There is no limit on how long such a request waits.
- **Pinned workers:** threads and child processes started from a handler inherit the CPU pin. The worker count follows the affinity mask rather than a cgroup CPU quota, so a container with a quota gets one worker per visible CPU. A server started from a thread pinned to one CPU gets one worker.
- **Connection resets:** client data that arrives between the check for unread data and the close still resets the connection, on both platforms. On Linux this only happens once the whole response has gone out, so what can be lost is a segment that needed resending. On Windows the response can still be in the socket's buffer. The checks cost two `ioctl` calls per connection on Linux and one `ioctlsocket` on Windows.
- **Lingering connections on Linux:** a connection waiting for the client to take the end of its response holds its file descriptor until the client closes, everything is acknowledged, or 30s pass without an acknowledgement. Each pass that discards client data makes one more `ioctl`.
- **Lingering connections on Windows:** Windows has no cheap way to ask how much the client has acknowledged, so a lingering connection is closed only when the client closes or after 30s. A client that reads its response to the end and then leaves the connection open holds a socket for those 30s.
- **Rescue limits:** connections behind a stalled worker stay in the kernel's queue while every parked worker is busy. The count of free workers is read once per check, so a worker woken by another connection in between leaves a rescued connection waiting for the next free one.
- **Slow clients on Windows:** a client taking its response at under about 550 bytes a second is closed as having stopped reading.
- **Lock contention on Windows:** every accepted connection, partial receive and handled request takes the shared last-activity lock.

## Performance

`cargo bench --bench throughput`: time per request for a 4KB POST with a 4KB response. Measured on 16 CPUs under WSL2, on Linux only. A virtual machine can make cross-CPU wake-ups unusually expensive, and these runs weren't repeated on native Linux, so the gaps may differ there.

Results vary a lot between runs on this machine, so these are medians of alternating runs, with the range in brackets. The runs come from several sessions, so the ranges include differences between sessions.

| Listener | Runs | 1 client | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|---|
| Before the audit fixes (`988521c`) | 5 | 23.2 µs (21.7–23.9) | 8.9 µs (8.6–12.2) | 8.3 µs (7.4–13.5) | 7.9 µs (6.7–8.8) | 7.2 µs (6.0–10.5) |
| All fixes, but keeping `SCHED_IDLE` | 6 | 24.1 µs (21.8–27.5) | 8.9 µs (8.8–13.3) | 8.7 µs (7.5–10.3) | 8.6 µs (7.1–10.9) | 7.3 µs (6.7–15.7) |
| **All fixes (current)** | 6 | **46.3 µs** (44.1–54.4) | **26.4 µs** (23.8–33.9) | **20.1 µs** (18.9–23.3) | **11.5 µs** (9.5–14.2) | **11.6 µs** (9.1–12.6) |
| All fixes, with `SCHED_BATCH` | 5 | 46.4 µs (45.2–66.2) | 24.5 µs (22.7–35.2) | 19.8 µs (18.7–30.2) | 13.6 µs (10.0–14.5) | 11.1 µs (8.9–13.0) |

- **Almost all the cost comes from dropping `SCHED_IDLE`.** The rows with and without the other fixes overlap.
- **`SCHED_IDLE` can't stay:** a CPU-heavy handler sharing 2 CPUs with a busy process pinned to each took up to 26s before the fixes, and up to 11.7s with the fixes but `SCHED_IDLE` kept. The current code took at most 216ms over 200 requests.
- **`SCHED_BATCH` isn't worth it:** it doesn't clearly beat the default policy on the benchmark, and a fast request on those busy CPUs had a median of 4ms against 0.08ms.

### Cost of the second audit fixes

Five alternating runs of the code before and after the second audit fixes, in one session, as medians with the range in brackets:

| Listener | Runs | 1 client | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|---|
| Before the second audit fixes (`39ec2b7`) | 5 | 48.7 µs (42.0–50.9) | 24.2 µs (23.0–25.3) | 18.4 µs (17.5–23.8) | 10.6 µs (9.8–14.8) | 9.4 µs (8.7–13.8) |
| **After (current)** | 5 | **55.1 µs** (47.8–66.5) | **29.1 µs** (27.4–42.1) | **21.9 µs** (21.5–28.5) | **14.3 µs** (13.5–18.1) | **10.5 µs** (10.4–14.6) |

- **Each request takes about 10 to 35% longer.** The ranges overlap at 1 and 16 clients and are separate at 4 and 12.
- **The likely cause is dropping `MSG_MORE` and the fast close,** which adds a packet or two to each connection and leaves each one in `TIME_WAIT`. The two weren't measured separately, and the framing and rescue changes weren't either.

### Cost of the third audit fixes

Five alternating runs of the code before and after the third audit fixes, in one session, as medians with the range in brackets. The diagrams and this file were being edited on the same machine during the runs.

| Listener | Runs | 1 client | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|---|
| Before the third audit fixes (`46c9b96`) | 5 | 46.9 µs (44.6–51.7) | 25.9 µs (25.5–27.8) | 21.2 µs (20.4–22.6) | 13.2 µs (12.6–17.2) | 10.0 µs (9.8–14.0) |
| **After (current)** | 5 | **45.9 µs** (45.0–47.7) | **26.4 µs** (25.6–32.9) | **20.9 µs** (20.7–25.5) | **14.1 µs** (13.1–15.6) | **10.2 µs** (9.8–13.3) |

- **No difference shows.** The ranges overlap at every client count, and the medians differ by 7% at most, in both directions.
- **The extra `ioctl` after the last `send` is too small to see here.** This benchmark's 4KB response always goes out at once, so it never takes the lingering close, and that path wasn't measured.

### Cost of the fourth audit fixes

Five alternating runs of the code before and after the fourth audit fixes, in one session, as medians with the range in brackets. The diagrams and this file were being edited on the same machine during the runs. The first "before" run produced no result at 4 and 12 clients, so those two cells are from 4 runs.

| Listener | Runs | 1 client | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|---|
| Before the fourth audit fixes (`e2f6992`) | 5 (4 at 4 and 12 clients) | 47.7 µs (46.5–52.0) | 26.7 µs (25.7–28.3) | 21.1 µs (20.7–28.4) | 13.7 µs (13.1–16.7) | 11.5 µs (10.0–12.3) |
| **After (current)** | 5 | **48.5 µs** (44.5–54.3) | **25.7 µs** (25.3–31.9) | **20.7 µs** (20.5–24.0) | **13.1 µs** (12.7–13.4) | **10.0 µs** (9.6–10.6) |

- **No slowdown shows.** The ranges overlap at every client count. The medians differ by 5% at most up to 12 clients, and at 16 clients the current code's median is 13% lower, which is within the spread between runs seen in the earlier tables.
- **The second `ioctl` per connection (`FIONREAD`) is too small to see here.** This benchmark sends nothing after its request and its 4KB response always goes out at once, so it never takes the lingering close, and that path wasn't measured.

The shared pool design, kept on the `tcp-listener-shared-pool` branch, was measured over 2 runs each, alternated with the code before the audit fixes and the current code:

| Shared pool | 1 client | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|
| 32 accept workers | 61.6–63.5 µs | 41.1–56.2 µs | 28.8–41.7 µs | 28.1–39.7 µs | 28.2–33.3 µs |
| 4 accept workers | 56.6–62.4 µs | 28.9–35.8 µs | 19.4–27.9 µs | 14.4–22.0 µs | 13.4–22.2 µs |

## Tests

The branch adds 42 tests:

| Location | Test | Commit |
|---|---|---|
| `src/tcp_listener.rs` | `request_len_finds_crlf_and_bare_lf_blank_lines` | `e0f62d7`, renamed from `header_end_…` |
| `src/tcp_listener.rs` | `request_len_ends_the_head_where_the_parser_does` | `e0f62d7`, replaces `header_end_needs_a_cr_before_a_crlf_blank_line` |
| `src/tcp_listener.rs` | `request_len_stops_at_the_first_blank_line` | `e0f62d7`, renamed from `header_end_…` |
| `src/tcp_listener.rs` | `request_len_frames_by_content_length` | `e0f62d7` |
| `src/tcp_listener.rs` | `request_len_rejects_oversized_requests` | `e0f62d7` |
| `tests/tcp_roundtrip.rs` | `more_idle_connections_than_cores_do_not_block_other_requests` | `e0f62d7` |
| `tests/tcp_roundtrip.rs` | `idle_connection_is_closed_after_timeout` | `e0f62d7` |
| `tests/tcp_roundtrip.rs` | `second_server_cannot_bind_a_port_in_use` | `e0f62d7` |
| `tests/tcp_roundtrip.rs` | `panicking_handler_does_not_stop_the_server` | `16445a3` |
| `tests/tcp_roundtrip.rs` | `large_request_and_response_arrive_whole` | `16445a3` |
| `src/tcp_listener.rs` | `request_len_applies_the_header_limit_to_a_complete_head` | Audit fixes |
| `src/tcp_listener.rs` | `request_len_frames_chunked_bodies` | Audit fixes |
| `src/tcp_listener.rs` | `request_len_hands_malformed_chunks_to_the_parser` | Audit fixes |
| `src/tcp_listener.rs` | `request_len_rejects_oversized_chunked_bodies` | Audit fixes |
| `src/tcp_listener.rs` | `request_len_ignores_other_transfer_codings` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `second_of_two_servers_bound_together_fails_to_run` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `request_sent_in_two_writes_is_not_held_up_by_a_delayed_ack` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `slow_handlers_do_not_hold_up_an_open_connection` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `large_response_survives_a_client_pause` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `oversized_headers_are_dropped` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `chunked_body_is_received_and_decoded` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `chunked_body_split_across_writes_is_reassembled` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `malformed_chunked_body_gets_400` | Audit fixes |
| `tests/tcp_roundtrip.rs` | `non_utf8_body_is_available_byte_for_byte` | Audit fixes |
| `src/tcp_listener.rs` | `request_len_hands_headers_the_parser_rejects_straight_to_it` | Second audit fixes |
| `src/tcp_listener.rs` | `framer_keeps_its_place_between_receives` | Second audit fixes |
| `src/tcp_listener.rs` | `framer_reads_each_chunk_of_a_body_of_tiny_chunks_once` | Second audit fixes |
| `tests/tcp_roundtrip.rs` | `non_utf8_header_leaves_the_body_as_it_was_sent` | Second audit fixes |
| `tests/tcp_roundtrip.rs` | `chunked_with_content_length_gets_400_straight_away` | Second audit fixes |
| `tests/tcp_roundtrip.rs` | `head_is_framed_the_way_the_parser_reads_it` | Second audit fixes |
| `tests/tcp_roundtrip.rs` | `response_is_sent_before_a_close_with_unread_data` | Second audit fixes |
| `tests/tcp_roundtrip.rs` | `requests_waiting_for_a_worker_are_not_closed_as_idle` (Linux) | Second audit fixes |
| `src/tcp_listener.rs` | `framer_reads_a_chunked_body_with_an_unfinished_line_once` | Third audit fixes |
| `src/tcp_listener.rs` | `framer_refuses_anything_but_a_line_ending_after_chunk_data` | Third audit fixes |
| `tests/tcp_roundtrip.rs` | `large_response_survives_a_slow_steady_reader` | Third audit fixes |
| `tests/tcp_roundtrip.rs` | `client_that_stops_reading_is_closed` | Third audit fixes |
| `tests/tcp_roundtrip.rs` | `large_response_arrives_whole_with_unread_data` | Third audit fixes |
| `tests/tcp_roundtrip.rs` | `idle_connections_do_not_stop_requests_being_taken_from_a_busy_worker` (Linux) | Third audit fixes |
| `tests/tcp_roundtrip.rs` | `every_worker_takes_its_share_of_queued_requests` (Linux) | Third audit fixes |
| `src/tcp_listener.rs` | `receive_buffer_only_grows_once_it_is_nearly_full` | Fourth audit fixes |
| `src/tcp_listener.rs` | `head_len_ends_the_head_where_the_framer_does` | Fourth audit fixes |
| `tests/tcp_roundtrip.rs` | `client_that_floods_while_its_response_lingers_is_closed` (Linux) | Fourth audit fixes |

`idle_connection_does_not_block_other_requests` and `more_idle_connections_than_cores_do_not_block_other_requests` were also corrected. `TCP_DEFER_ACCEPT` meant the silent sockets hadn't been accepted when they checked, so they weren't testing idle handling on Linux.

The second audit fixes also changed two existing tests. `header_end_needs_a_cr_before_a_crlf_blank_line` asserted that a head ending in `\n\r\n` was incomplete, which is the disagreement with the parser that finding 7 reported, so it now asserts the opposite. `large_response_survives_a_client_pause` is no longer limited to Linux, so the Windows CI job covers the 30s send timeout.

The five new round-trip tests all fail on the code before the second audit fixes and pass after them.

Of the seven tests added with the third audit fixes, six fail on the code before them (`46c9b96`) and pass after. `client_that_stops_reading_is_closed` passes on both: it guards the 30s timeout that the new progress check could have broken. Two of the tests take about 35s each, since they have to outlast that timeout, and the test binary runs them alongside the rest.

The fourth audit fixes also changed three existing tests:
- `response_is_sent_before_a_close_with_unread_data` now requires that the client sees no reset at all. Before, it only required that the response arrived ahead of the reset.
- It and `large_response_arrives_whole_with_unread_data` are no longer limited to Linux, so the Windows CI job covers the lingering close there.
- `large_response_survives_a_client_pause` uses the shared `start_large_server` and `body_len` helpers, and the three copies of the CPU-pinning block are now one `pin_to_current_cpu` helper.

`client_that_floods_while_its_response_lingers_is_closed` and the stricter `response_is_sent_before_a_close_with_unread_data` both fail on the code before the fourth audit fixes (`e2f6992`) and pass after. The first takes about 30s. The two new unit tests cover shared helpers and weren't run against the old code, which doesn't have those helpers.

### What was checked

- **Linux:** `cargo test --workspace --all-targets` passes all 130 tests, and `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- **Linux, by hand:**
  - A CPU-heavy handler sharing 2 CPUs with a busy process pinned to each took a median of 185ms and at most 216ms over 200 requests, with no stalls.
  - At the file descriptor limit of 64, with 100 connections opened at once, the server used 0–4% of a core and logged 4 accept errors over 8s. It accepted connections again once the idle sweep freed descriptors.
  - Port 0 opened 16 sockets on one port.
  - An idle server made 20 voluntary context switches across all its threads in 10s, against 977 before the second audit fixes.
  - With logging on and stdout pointed at `/dev/full`, the server answered 40 of 40 requests. The code before the fourth audit fixes panicked on its first log line and exited.
- **Windows:** the library compiles and passes clippy for `x86_64-pc-windows-gnu` (`cargo clippy --lib --target x86_64-pc-windows-gnu -- -D warnings`). Nothing was run on Windows, and the tests and benches weren't compiled for it, because their dependencies need a Windows C compiler that isn't installed on the machine used. The send cap (now 16KB), the 30s send timeout, the empty-response change and the lingering close added by the fourth audit fixes are therefore untested, including over a real network path, which is where finding 3 of `audit_mc1.md` applies. The lingering close is the largest of these: it adds `shutdown`, `SO_UPDATE_ACCEPT_CONTEXT` and a new overlapped operation, none of which has run. The `test-windows` and Windows clippy CI jobs cover the rest on pull requests to `main`.
- **Not tested:** the rescue thread's limit of 64 accepts per socket per check has no automated test, since the scenario needs a stalled worker and a sustained flood of connections. Moving the sweep's logging outside the lock, and steering on architectures other than x86-64, were not tested either. From the fourth audit fixes, `AbortOnPanic` has no test, since triggering it ends the test process, and the loss of a response segment in transit (finding 4 of `audit_mc3.md`) wasn't reproduced: the test checks only that the connection is no longer reset.

## Documentation

The three Visio diagrams (`Linux TCP request handler sequence diagram.vsdx`, `Linux TCP startup sequence diagram.vsdx` and `Linux request lifecycle and thread states.vsdx`) were updated for the second and third audit fixes by editing the exported files directly. For the third, only the text of existing shapes was changed: no step was added, removed or renumbered, so the lingering close appears as wording on the existing close steps and not as steps of its own. The fourth audit fixes were added the same way, as changed text on 19 existing shapes. They weren't opened in Visio afterwards, so text that got longer may no longer fit its shape, and any Lucidchart originals they were exported from are unchanged.

`README.md` now mentions epoll, the completion port and `AcceptEx`. It also corrects the throughput benchmark's client counts to 1, 4, 8, 12 and 16, which match `benches/throughput.rs`.

## Not included

These uncommitted files in `scripts/` are not part of the branch's history, so they aren't covered above: `builder.sh` (modified), `build.rs`, `build.sh` and `views_mod.rs` (all untracked). Finding 10 of `audit_mc3.md` is that they are unfinished stubs that don't match the crate, and its fix is to keep them out of this branch, so they were left as they are.
