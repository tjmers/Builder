# Builder

A rust backend framework that optimizes for speed by being efficient and giving users clear implementations allowing for quick development.

## Supported Architectures

Builder requires a 64 bit linux or windows environment

## Implementation details

The sockets are specifically implemented via os-specific sockets because they will later be optimized for speed and the os-control is desired.

The shared request framing lives in `src/tcp_listener.rs`, and each platform supplies its own socket calls: `src/tcp_listener/linux.rs` uses libc and epoll, and `src/tcp_listener/windows.rs` uses Winsock through windows-sys, with a completion port and `AcceptEx`.

## Benchmarks

`cargo bench --bench throughput` measures end to end throughput (requests/s) over loopback sockets, with 1, 4, 8, 12, and 16 concurrent clients sending a 4kb `POST /` and receiving a 4kb `200` response. Reports are written to `target/criterion/`, and each run is compared against the previous one.

`cargo bench --bench compare` runs the same workload against builder and four reference servers in one criterion group: a minimal thread-per-connection responder on `std::net`, axum (tokio + hyper), actix-web, and tiny_http. Each one serves a prebuilt 4kb body and reads the whole request body first. Every request carries `Connection: close`, since builder closes after each response, so no server gets keep-alive as an advantage. The side by side report is `target/criterion/compare_post_4kb_resp_4kb/report/index.html`. Run a single server with `cargo bench --bench compare -- axum`. A full run takes about 6 minutes.

The client code both benches share is in `benches/common/mod.rs`.

## TODO

- Force crlf internally (src/tcp_listener.rs:38)