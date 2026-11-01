# Builder

A rust backend framework that optimizes for speed by being efficient and giving users clear implementations allowing for quick development.

## Supported Architectures

Builder requires a 64 bit linux or windows environment

## Implementation details

The sockets are specifically implemented via os-specific sockets because they will later be optimized for speed and the os-control is desired.

The shared request framing lives in `src/tcp_listener.rs`, and each platform supplies its own socket calls: `src/tcp_listener/linux.rs` uses libc and `src/tcp_listener/windows.rs` uses Winsock through windows-sys.


## TODO

- Force crlf internally (src/tcp_listener.rs:38)