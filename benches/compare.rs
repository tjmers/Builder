// Compares builder's end to end throughput against other servers, all running the same client workload as the
// throughput bench: one connection per request, a 4kb POST / answered with a cached 4kb 200
// Every server reads the whole request body before responding (as builder does), and every request carries
// Connection: close, since builder closes after each response and the others would otherwise keep the connection

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::server::Server;

use common::{BODY_BYTES, CLIENT_COUNTS};

// The response body every server sends, built once
static BODY: [u8; BODY_BYTES] = [b'a'; BODY_BYTES];

// Each server's name in the report and its port, outside the 381xx range used by the tests and the throughput bench
const SERVERS: [(&str, u16, fn(u16)); 5] = [
    ("builder", 38202, start_builder),
    ("std", 38203, start_std),
    ("axum", 38204, start_axum),
    ("actix", 38205, start_actix),
    ("tiny_http", 38206, start_tiny_http),
];

// Every start_* function serves on port from a background thread that is never joined,
// so it ends when the bench process exits

fn start_builder(port: u16) {
    thread::spawn(move || {
        // Built once and served from the state, so the bench measures the server rather than the handler
        let cached = HttpResponseBuilder::from_text(Status::Ok, "a".repeat(BODY_BYTES)).build();
        let mut s = Server::new(cached);
        s.add_endpoint(Method::Post, "/", |_request: &HttpRequest, state: &HttpResponse<'static>| -> HttpResponse {
            state.borrowed()
        });
        s.run(port as i32);
    });
}

// The floor: a minimal blocking HTTP responder on std::net with a thread per connection
fn start_std(port: u16) {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {BODY_BYTES}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    response.extend_from_slice(&BODY);
    let response: &'static [u8] = response.leak();

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind std server");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            thread::spawn(move || std_serve(stream, response));
        }
    });
}

// Reads one whole request (headers, then Content-Length bytes of body), then sends response and closes
fn std_serve(mut stream: TcpStream, response: &[u8]) {
    let mut buf = Vec::with_capacity(2 * BODY_BYTES);
    let mut chunk = [0u8; 8192];
    let headers_end = loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let content_length = std::str::from_utf8(&buf[..headers_end])
        .unwrap_or("")
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < headers_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let _ = stream.write_all(response);
}

fn start_axum(port: u16) {
    use axum::body::Bytes;
    use axum::routing::post;

    // Taking the body as Bytes makes axum read all of it
    async fn handler(_body: Bytes) -> ([(&'static str, &'static str); 1], Bytes) {
        ([("content-type", "text/plain")], Bytes::from_static(&BODY))
    }

    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind axum server");
            let app = axum::Router::new().route("/", post(handler));
            axum::serve(listener, app).await.expect("axum server");
        });
    });
}

fn start_actix(port: u16) {
    use actix_web::{web, App, HttpResponse, HttpServer};

    // Taking the body as Bytes makes actix read all of it
    async fn handler(_body: web::Bytes) -> HttpResponse {
        HttpResponse::Ok().content_type("text/plain").body(&BODY[..])
    }

    thread::spawn(move || {
        actix_web::rt::System::new().block_on(async move {
            HttpServer::new(|| App::new().route("/", web::post().to(handler)))
                .bind(("127.0.0.1", port))
                .expect("bind actix server")
                .run()
                .await
                .expect("actix server");
        });
    });
}

fn start_tiny_http(port: u16) {
    let server = Arc::new(tiny_http::Server::http(("127.0.0.1", port)).expect("bind tiny_http server"));
    let content_type = tiny_http::Header::from_bytes("Content-Type", "text/plain").unwrap();
    // tiny_http hands requests out from one queue, so one thread per CPU takes them from it
    let threads = thread::available_parallelism().map_or(4, |n| n.get());
    for _ in 0..threads {
        let server = Arc::clone(&server);
        let content_type = content_type.clone();
        thread::spawn(move || {
            let mut discard = Vec::with_capacity(BODY_BYTES);
            for mut request in server.incoming_requests() {
                discard.clear();
                let _ = request.as_reader().read_to_end(&mut discard);
                let response = tiny_http::Response::new(
                    tiny_http::StatusCode(200),
                    vec![content_type.clone()],
                    &BODY[..],
                    Some(BODY_BYTES),
                    None,
                );
                let _ = request.respond(response);
            }
        });
    }
}

fn compare_post_4kb_resp_4kb(c: &mut Criterion) {
    let request = common::request_bytes("Connection: close\r\n");
    for (_, port, start) in SERVERS {
        start(port);
    }
    for (_, port, _) in SERVERS {
        common::wait_until_listening(port);
        common::check_response(port, &request);
    }

    let mut group = c.benchmark_group("compare_post_4kb_resp_4kb");
    // Reported as requests per second
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    for (name, port, _) in SERVERS {
        for clients in CLIENT_COUNTS {
            group.bench_with_input(BenchmarkId::new(name, clients), &clients, |b, &clients| {
                b.iter_custom(|iters| common::run_clients(port, &request, clients, iters));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, compare_post_4kb_resp_4kb);
criterion_main!(benches);
