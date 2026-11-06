# Changes on `tcp-listener-optimization`

This branch replaces the server's one-thread-per-connection model with a fixed pool of worker threads that use each OS's native event system: epoll on Linux and I/O completion ports on Windows. It also makes HTTP request parsing cheaper by checking the request body only when a handler reads it.

| | |
|---|---|
| **Compared against** | `3a72253`, where the branch split from `main` |
| **Commits** | `e0f62d7` (Linux and parsing), `16445a3` (Windows) |
| **Size** | 8 files, +1581 / -307 lines, before the audit fixes |
| **Audit fixes** | The fixes for `audit.md`, not committed yet |

## Before and after

| | Before | Linux now | Windows now |
|---|---|---|---|
| **Threads** | One new thread per connection | One pinned worker per CPU, `max(CPUs, 4)` parked workers, plus a rescue and sweep thread | Two workers per core, plus a pool and sweep thread |
| **Socket I/O** | Blocking `recv` and `send` | Non-blocking, with epoll for connections that have to wait | Overlapped `WSARecv` and `WSASend`, with a completion port |
| **Accepting** | One blocking `accept` loop | Each worker has its own `SO_REUSEPORT` socket and waits in a blocking `accept4` | A pool of 64 outstanding `AcceptEx` calls |
| **Idle timeout** | 5s per `recv` (`SO_RCVTIMEO`) | Swept every second: 5s while receiving, 30s while sending | Swept every second, using `CancelIoEx` |
| **Handler panic** | Not caught | Caught; only that connection is lost | Caught; only that connection is lost |

An idle connection, such as a browser's unused preconnect socket, now takes up a map entry instead of a thread.

## Linux listener (`src/tcp_listener/linux.rs`)

### Threads
- **Pinned workers:** one per CPU the calling thread may use, each pinned to that CPU with its own listening socket. `SO_INCOMING_CPU` steers connections to the worker on the CPU that handled their packets. The usual path is `accept4`, `recv`, `send`, `close`, with no epoll calls and no shared lock. In a counted benchmark run with `SCHED_IDLE` still on, 32 of 5.4 million connections left this path.
- **Parked workers:** `max(CPUs, 4)` unpinned threads waiting in `epoll_wait` on one shared epoll, taking one event at a time. Taking a batch would leave the rest waiting behind that thread's handler.
- **Rescue thread:** the calling thread checks every 10ms for a worker that has made no progress while connections wait on its socket, and moves those connections to the parked workers. It never runs a handler, so a slow handler can't stop it. It also closes idle connections once a second.

Workers run under the default scheduling policy. `SCHED_IDLE` starved handlers on a busy CPU, and `SCHED_BATCH` made fast requests about 50 times slower there (see [Performance](#performance)).

### Parking a connection
- A connection that can't finish without blocking is put in a map behind a mutex and registered with `EPOLLONESHOT`. It goes in the map first, because its event can fire before `epoll_ctl` returns.
- A parked worker takes the connection out of the map while driving it, so only one thread holds it at a time and the sweep can't close it.
- The lock is held across the map insert and `epoll_ctl`, so the sweep can't close the fd in between.
- An event with no map entry is ignored. A stale event that finds a new connection on a reused fd is harmless, because that worker finds nothing to read and parks it again.
- Requests are received straight into the request buffer, with no intermediate chunk copy or zeroing. Each worker also keeps one finished connection's buffer to reuse.

### Taking over from a stalled worker
The rescue thread makes the socket non-blocking only while it empties the queue, so it can't get stuck in `accept` if the worker wakes and takes the connections first. A worker already blocked in `accept` stays blocked, and one that arrives during that window gets `EAGAIN` and tries again.

### Running out of file descriptors
When `accept4` fails with `EMFILE`, `ENFILE`, `ENOBUFS` or `ENOMEM`, the worker sleeps for 100ms before trying again and logs once per run of failures. The rescue thread pauses for the same 100ms.

### Socket options

| Option | Effect |
|---|---|
| `SO_REUSEPORT` | Gives each pinned worker its own listening socket on the same port |
| `SO_INCOMING_CPU` | Sends a connection to the socket of the worker on the CPU that handled its packets |
| `TCP_DEFER_ACCEPT` (1s) | Holds a connection back from `accept` until its first data arrives, so the worker is woken only once |
| `TCP_LINGER2` (-1) | Skips `TIME_WAIT` after the server closes; responses over 64KB opt out and get a normal close |
| `MSG_MORE` on send | Lets the FIN share the last data packet |
| `SOCK_CLOEXEC` | Stops sockets being inherited by child processes |

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
- Last-activity times are kept in one `Mutex<HashMap>` shared by all workers.
- `Cargo.toml` adds three `windows-sys` features: `Win32_Foundation`, `Win32_Storage_FileSystem` and `Win32_System_IO`.

## Shared listener code (`src/tcp_listener.rs`)

- **Added:** `Framing` and `request_len`, which apply the same size limits and request framing on both platforms, and `log_closed_early`.
- **Header limit:** a head over 8KB is now refused even when it arrives complete in one receive.
- **Chunked framing:** `request_len` now frames chunked bodies when the first `Transfer-Encoding` header is `chunked`. Malformed chunks are handed to the parser, which answers `400`. The 1MB limit counts the body with its chunk framing.
- **Removed:** `recv_chunk`, `recv_request`, `send_all`, `serve`, `serve_connection`, `RECV_CHUNK_BYTES` and the `platform` alias.
- **Rewritten:** `header_end` now stops at the first blank line instead of scanning the entire buffer, including the body.
- **Changed:** `IDLE_TIMEOUT` is now `pub(crate)`.

## HTTP parsing (`src/http_request.rs`, `src/server.rs`)

- **`HttpRequest::from_bytes` (new, `pub(crate)`):** only the request line and headers must be valid UTF-8 up front.
- **Lazy body text:** `content()` still returns `&str`, but converts the body on first call and caches it in a `OnceLock`.
- **`Server::handle_raw`:** now uses `from_bytes`. If the head isn't valid UTF-8, it falls back to the old lossy conversion and `HttpRequest::new`.
- **New `Head` struct:** collects `Host`, `Content-Length` and `Transfer-Encoding` during the single pass over the headers. This replaces `parse_body` and the later header re-scans.
- **`HttpRequest::body` (new, public):** returns the body's bytes as sent, with any chunked encoding removed.
- **`walk_chunks` (new, `pub(crate)`):** the chunk walk shared by framing and `decode_chunked`, so the two can't disagree about where a body ends.
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
3. **Stalled sends:** a client that stops reading its response is now disconnected. Before, a blocking `send` had no timeout. The limit is 30s without progress on Linux and the 5s idle timeout on Windows.
4. **Idle timing:** connections now close after 5–6 seconds instead of 5. On Linux, `TCP_DEFER_ACCEPT` can add about another second before a silent connection is accepted and its timer starts.
5. **Handler panics:** the connection is closed with no response, and the worker keeps running.
6. **Windows logging:** the `Connection from` line no longer includes the client's port.
7. **Large headers:** a head over 8KB is now dropped even when it arrives complete. Before, the limit only applied while the head was still incomplete.

The public `TcpListener` and `HttpRequest` methods keep their signatures, and `HttpRequest` gains `body()`.

## Trade-offs

- **Linux throughput:** see [Performance](#performance). Each request takes about 1.5 to 3 times as long as before the audit fixes on this benchmark, nearly all from dropping `SCHED_IDLE`.
- **Slow handlers on Linux:** a slow handler takes up one pinned worker, and new connections queued on its socket wait up to about 20ms for the rescue thread. If every parked worker is also in a slow handler, those connections wait for one to come free, and the idle sweep closes any still waiting after 5–6s with no response, even though their requests have arrived.
- **Pinned workers:** threads and child processes started from a handler inherit the CPU pin. The worker count follows the affinity mask rather than a cgroup CPU quota, so a container with a quota gets one worker per visible CPU. A server started from a thread pinned to one CPU gets one worker.
- **Connection resets on Linux:** with `TCP_LINGER2` at -1, the client's final ACK is answered with a reset. A client that discards unread data on a reset could lose the end of a response, which is why responses over 64KB opt out.
- **Chunked framing cost:** `request_len` walks the chunks from the start after every receive, so a client sending a large body of tiny chunks in small pieces costs more CPU than one using `Content-Length`. The 1MB limit bounds each walk.
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

The shared pool design, kept on the `tcp-listener-shared-pool` branch, was measured over 2 runs each, alternated with the code before the audit fixes and the current code:

| Shared pool | 1 client | 4 | 8 | 12 | 16 |
|---|---|---|---|---|---|
| 32 accept workers | 61.6–63.5 µs | 41.1–56.2 µs | 28.8–41.7 µs | 28.1–39.7 µs | 28.2–33.3 µs |
| 4 accept workers | 56.6–62.4 µs | 28.9–35.8 µs | 19.4–27.9 µs | 14.4–22.0 µs | 13.4–22.2 µs |

## Tests

The branch adds 24 tests:

| Location | Test | Commit |
|---|---|---|
| `src/tcp_listener.rs` | `header_end_finds_crlf_and_bare_lf_blank_lines` | `e0f62d7` |
| `src/tcp_listener.rs` | `header_end_needs_a_cr_before_a_crlf_blank_line` | `e0f62d7` |
| `src/tcp_listener.rs` | `header_end_stops_at_the_first_blank_line` | `e0f62d7` |
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

`idle_connection_does_not_block_other_requests` and `more_idle_connections_than_cores_do_not_block_other_requests` were also corrected. `TCP_DEFER_ACCEPT` meant the silent sockets hadn't been accepted when they checked, so they weren't testing idle handling on Linux.

### What was checked

- **Linux:** `cargo test --workspace --all-targets` passes all 112 tests.
- **Linux, by hand:**
  - A CPU-heavy handler sharing 2 CPUs with a busy process pinned to each took a median of 185ms and at most 216ms over 200 requests, with no stalls.
  - At the file descriptor limit of 64, with 100 connections opened at once, the server used 0–4% of a core and logged 4 accept errors over 8s. It accepted connections again once the idle sweep freed descriptors.
  - Port 0 opened 16 sockets on one port.
- **Windows:** not compiled or run for this summary, because the Windows target isn't installed on the machine used. That code is described from the diff only. The shared framing and parsing changes also reach Windows, and the `test-windows` CI job covers them on pull requests to `main`.

## Documentation

`README.md` now mentions epoll, the completion port and `AcceptEx`. It also corrects the throughput benchmark's client counts to 1, 4, 8, 12 and 16, which match `benches/throughput.rs`.

## Not included

These uncommitted files in `scripts/` are not part of the branch's history, so they aren't covered above: `builder.sh` (modified), `build.rs`, `build.sh` and `views_mod.rs` (all untracked).
