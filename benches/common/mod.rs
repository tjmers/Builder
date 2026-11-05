// The client side shared by the benches: each iteration is one full connection over loopback
// connect, send the request, read the response until the server closes

// Each bench uses a different subset of these
#![allow(dead_code)]

use std::hint::black_box;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

pub const BODY_BYTES: usize = 4096;
// Number of clients sending requests at the same time
pub const CLIENT_COUNTS: [usize; 5] = [1, 4, 8, 12, 16];

// Retries until the server on port accepts a connection
pub fn wait_until_listening(port: u16) {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("server never started listening on port {port}");
}

// A POST / with a BODY_BYTES body, with extra_headers (each ending in \r\n) added after the standard ones
pub fn request_bytes(extra_headers: &str) -> Vec<u8> {
    let mut request =
        format!("POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {BODY_BYTES}\r\n{extra_headers}\r\n").into_bytes();
    request.resize(request.len() + BODY_BYTES, b'a');
    request
}

// Sends request on a new connection and reads the whole response into buf, returning its length
pub fn round_trip(port: u16, request: &[u8], buf: &mut Vec<u8>) -> usize {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.write_all(request).expect("write request");
    buf.clear();
    stream.read_to_end(buf).expect("read response");
    buf.len()
}

// Fails loudly if the server is not answering with the expected 200 and 4kb body
pub fn check_response(port: u16, request: &[u8]) {
    let mut buf = Vec::new();
    round_trip(port, request, &mut buf);
    assert!(
        buf.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "response from port {port}: {:?}",
        String::from_utf8_lossy(&buf)
    );
    let body_start = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has no end of headers")
        + 4;
    assert_eq!(buf.len() - body_start, BODY_BYTES, "unexpected response body length from port {port}");
}

// Sends iters requests split across clients threads, and returns the wall clock time they took
pub fn run_clients(port: u16, request: &[u8], clients: usize, iters: u64) -> Duration {
    let clients_u64 = clients as u64;
    // Everyone (the clients and this thread) waits here, so spawning threads is not measured
    let start_line = Barrier::new(clients + 1);
    thread::scope(|scope| {
        for i in 0..clients_u64 {
            // The first iters % clients clients take one extra request
            let share = iters / clients_u64 + u64::from(i < iters % clients_u64);
            let start_line = &start_line;
            scope.spawn(move || {
                let mut buf = Vec::with_capacity(2 * BODY_BYTES);
                start_line.wait();
                for _ in 0..share {
                    black_box(round_trip(port, black_box(request), &mut buf));
                }
            });
        }
        start_line.wait();
        let start = Instant::now();
        // Leaving the scope joins every client
        start
    })
    .elapsed()
}
