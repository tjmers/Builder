// End to end throughput of Server::run over real loopback sockets
// Each iteration is one full connection: connect, send the request, read the response until the server closes

mod common;

use std::thread;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::server::Server;

use common::{BODY_BYTES, CLIENT_COUNTS};

// Outside the 381xx range used by the tests
const PORT: u16 = 38201;

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

fn post_4kb_resp_4kb(c: &mut Criterion) {
    start_server();
    common::wait_until_listening(PORT);
    let request = common::request_bytes("");
    common::check_response(PORT, &request);

    let mut group = c.benchmark_group("post_4kb_resp_4kb");
    // Reported as requests per second
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    for clients in CLIENT_COUNTS {
        group.bench_with_input(BenchmarkId::from_parameter(clients), &clients, |b, &clients| {
            b.iter_custom(|iters| common::run_clients(PORT, &request, clients, iters));
        });
    }
    group.finish();
}

criterion_group!(benches, post_4kb_resp_4kb);
criterion_main!(benches);
