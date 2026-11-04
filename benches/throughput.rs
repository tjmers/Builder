// End to end throughput of Server::run over real loopback sockets
// Each iteration is one full connection: connect, send the request, read the response until the server closes

use std::hint::black_box;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::server::Server;

// Outside the 381xx range used by the tests
const PORT: u16 = 38201;
const BODY_BYTES: usize = 4096;
// Number of clients sending requests at the same time
const CLIENT_COUNTS: [usize; 5] = [1, 4, 8, 12, 16];

// Starts the server on PORT in the background
// The thread is never joined, it ends when the bench process exits
fn start_server() {
    thread::spawn(|| {
        // Built once and served from the state, so the bench measures the server rather than the handler
        let cached = HttpResponseBuilder::from_text(Status::Ok, "a".repeat(BODY_BYTES)).build();
        let mut s = Server::new(cached);
        s.add_endpoint(Method::Post, "/", |_request: &HttpRequest, state: &HttpResponse<'static>| -> HttpResponse {
            state.borrowed()
        });
        s.run(PORT as i32);
    });
}

// Retries until the server accepts a connection
fn wait_until_listening() {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("server never started listening on port {PORT}");
}

fn request_bytes() -> Vec<u8> {
    let mut request = format!("POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {BODY_BYTES}\r\n\r\n").into_bytes();
    request.resize(request.len() + BODY_BYTES, b'a');
    request
}

// Sends request on a new connection and reads the whole response into buf, returning its length
fn round_trip(request: &[u8], buf: &mut Vec<u8>) -> usize {
    let mut stream = TcpStream::connect(("127.0.0.1", PORT)).expect("connect");
    stream.write_all(request).expect("write request");
    buf.clear();
    stream.read_to_end(buf).expect("read response");
    buf.len()
}

// Fails loudly if the server is not answering with the expected 200 and 4kb body
fn check_response(request: &[u8]) {
    let mut buf = Vec::new();
    round_trip(request, &mut buf);
    assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"), "response: {:?}", String::from_utf8_lossy(&buf));
    let body_start = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has no end of headers")
        + 4;
    assert_eq!(buf.len() - body_start, BODY_BYTES, "unexpected response body length");
}

// Sends iters requests split across clients threads, and returns the wall clock time they took
fn run_clients(request: &[u8], clients: usize, iters: u64) -> Duration {
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
                    black_box(round_trip(black_box(request), &mut buf));
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

fn post_4kb_resp_4kb(c: &mut Criterion) {
    start_server();
    wait_until_listening();
    let request = request_bytes();
    check_response(&request);

    let mut group = c.benchmark_group("post_4kb_resp_4kb");
    // Reported as requests per second
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    for clients in CLIENT_COUNTS {
        group.bench_with_input(BenchmarkId::from_parameter(clients), &clients, |b, &clients| {
            b.iter_custom(|iters| run_clients(&request, clients, iters));
        });
    }
    group.finish();
}

criterion_group!(benches, post_4kb_resp_4kb);
criterion_main!(benches);
