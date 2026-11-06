Regressions to fix

  Problem: CPU-heavy handlers can stall for many seconds: workers run under SCHED_IDLE, so any busy
    process on the same CPU starves them
  Where: src/tcp_listener/linux.rs:490
  Base: about 145ms
  Branch: 148ms, 243ms and 19.7s across 3 runs
  ────────────────────────────────────────
  Problem: Requests sent in two writes wait on a delayed ACK: turning TCP_QUICKACK off stalls clients
  that
    use Nagle (the default unless they set TCP_NODELAY)
  Where: src/tcp_listener/linux.rs:168
  Base: 0.3ms
  Branch: 43.8ms (median of 40)
  ────────────────────────────────────────
  Problem: Slow handlers hold up open connections on the same worker: the rescue thread only takes
    connections that haven't been accepted yet
  Where: src/tcp_listener/linux.rs:430
  Base: 0ms
  Branch: 5.8s
  ────────────────────────────────────────
  Problem: Two servers starting together silently share a port: both can bind before either one listens
  Where: src/tcp_listener/linux.rs:371
  Base: second server's run fails
  Branch: both run, splitting requests 108/92

  - Worst-case setups: the first test used a 72ms CPU handler on 2 CPUs with a busy process pinned to
    each. The third used one worker and four 3-second handlers, so with more workers it only affects
    connections on the blocked worker.
  - Simplest fixes: remove TCP_QUICKACK off and SCHED_IDLE; without SCHED_IDLE, you may not need the
    rescue thread either. This may give back some throughput, and I didn't run the benchmarks to measure
    that.
  - Behaviour change on Linux: a client that pauses for 10s now receives only 2.6MB of a 64MB response,
    where base delivered all of it. Your changes doc lists this as intended, but the same pause on
    Windows over loopback delivered all 64MB, so the two platforms now differ.

  Review claims that didn't reproduce

  - Windows clients losing responses to resets: the branch does send resets, but a Windows client got all
    of its responses in about 180 attempts, up to 60KB and with read delays up to 300ms. I only tried
    one Windows build over the WSL network, so other clients and proxies are untested. The 64KB cutoff at
    src/tcp_listener/linux.rs:38 guards against this same loss, which didn't occur here.
  - Windows cutting off slow readers: 64MB arrived complete at 4MB/s and 1MB/s, and after a 10s pause.
    That was loopback only, because the firewall blocked the test over a real network path, so this is
    still open.

  Older problems, also on base

  - Chunked requests: they always get a 400 over a socket, so decode_chunked is effectively unreachable.
  - Running out of file descriptors: the server spins at about one full core and floods the log.
  - Header limit: a 12KB header block is accepted despite the 8KB limit.
  - Binary bodies: non-UTF-8 bytes are silently replaced, and there's no way to read the raw body, which
    will block binary uploads.

  Risks for later

  I found these by reading the code, except port 0, which I ran.
  - Keep-alive: MSG_MORE, TCP_LINGER2 and the delayed ACK setting all assume the connection closes after
    each response, so they'd need rework if you add it.
  - Rescue thread: it can block for up to 1s in accept (src/tcp_listener/linux.rs:684), and a slow
    handler running on it stops all rescuing.
  - Inherited settings: threads or child processes started from a handler inherit the CPU pin and
    SCHED_IDLE.
  - Containers: the worker count follows the CPU affinity mask, not a cgroup CPU quota.
  - Port 0: each worker gets a different random port (2 workers opened 37709 and 41685).
  - Windows lock contention: one global mutex is taken on every accept, partial receive and handled
    request, and close_idle holds it across syscalls.
    request, and close_idle holds it across syscalls.
    request, and close_idle holds it across syscalls.

  Test gaps
  Test gaps

  - Idle-connection tests: idle_connection_does_not_block_other_requests and
    more_idle_connections_than_cores_do_not_block_other_requests don't test idle handling on Linux. They
    wait 50ms, but TCP_DEFER_ACCEPT meant a silent socket still hadn't been accepted after 500ms.
  - Windows coverage: the code compiles natively, and the 18 unit and round-trip tests I ran there pass,
    but that's only part of the 98 that pass on Linux.