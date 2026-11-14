// End to end throughput (requests/s) of every server over loopback
// Each server runs as its own process, one at a time, pinned away from the client's CPUs
// Every request carries Connection: close, since builder closes after each response, except in the reference group

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use framework_benches::{
    check_response, get_request, json_body_len, pin_client, post_request, run_clients, run_keep_alive_clients, Framework,
    ServerProc, PLAINTEXT, POST_BODY_BYTES,
};

// Number of clients sending requests at the same time
const CLIENT_COUNTS: [usize; 5] = [1, 4, 8, 12, 16];

// Every server on one workload, with a new connection per request
fn close_group(c: &mut Criterion, name: &str, request: &[u8], body_len: usize) {
    pin_client();
    let mut group = c.benchmark_group(name);
    // Reported as requests per second
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    for framework in Framework::ALL {
        let server = ServerProc::spawn(framework);
        let port = server.port();
        check_response(port, request, body_len);
        for clients in CLIENT_COUNTS {
            group.bench_with_input(BenchmarkId::new(framework.name(), clients), &clients, |b, &clients| {
                b.iter_custom(|iters| run_clients(port, request, clients, iters));
            });
        }
    }
    group.finish();
}

fn wire_plaintext(c: &mut Criterion) {
    close_group(c, "wire_plaintext", &get_request("/plaintext", true), PLAINTEXT.len());
}

fn wire_post_4kb(c: &mut Criterion) {
    close_group(c, "wire_post_4kb", &post_request(), POST_BODY_BYTES);
}

fn wire_json(c: &mut Criterion) {
    close_group(c, "wire_json", &get_request("/json", true), json_body_len());
}

// Reference only: what the others do on reused connections, the way they normally run
// Builder and the std baseline are absent because they close after every response
fn wire_plaintext_keepalive_reference(c: &mut Criterion) {
    pin_client();
    let request = get_request("/plaintext", false);
    let mut group = c.benchmark_group("wire_plaintext_keepalive_reference");
    group.throughput(Throughput::Elements(1));
    group.measurement_time(Duration::from_secs(10));
    for framework in [Framework::Axum, Framework::Actix, Framework::TinyHttp] {
        let server = ServerProc::spawn(framework);
        let port = server.port();
        for clients in CLIENT_COUNTS {
            group.bench_with_input(BenchmarkId::new(framework.name(), clients), &clients, |b, &clients| {
                b.iter_custom(|iters| run_keep_alive_clients(port, &request, clients, iters));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, wire_plaintext, wire_post_4kb, wire_json, wire_plaintext_keepalive_reference);
criterion_main!(benches);
