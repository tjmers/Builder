// Builder's parser and router against the libraries the other frameworks use, in process with no sockets

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::ip_address::IpAddress;
use builder::server::Server;

// ----- Parser -----

const SMALL: &str = "GET /plaintext HTTP/1.1\r\nHost: localhost\r\n\r\n";

const BROWSER: &str = "GET /app/dashboard/index.html?tab=overview HTTP/1.1\r\n\
Host: www.example.com\r\n\
Connection: keep-alive\r\n\
Cache-Control: max-age=0\r\n\
sec-ch-ua: \"Chromium\";v=\"130\", \"Not?A_Brand\";v=\"99\"\r\n\
sec-ch-ua-mobile: ?0\r\n\
sec-ch-ua-platform: \"Linux\"\r\n\
Upgrade-Insecure-Requests: 1\r\n\
User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36\r\n\
Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8\r\n\
Sec-Fetch-Site: same-origin\r\n\
Sec-Fetch-Mode: navigate\r\n\
Sec-Fetch-Dest: document\r\n\
Referer: https://www.example.com/app/\r\n\
Accept-Encoding: gzip, deflate, br\r\n\
Accept-Language: en-US,en;q=0.9\r\n\
Cookie: session=0123456789abcdef0123456789abcdef; theme=dark\r\n\r\n";

const PERCENT_ENCODED: &str =
    "GET /files/caf%C3%A9%20menu/2026%2F11/r%C3%A9sum%C3%A9%20final.pdf HTTP/1.1\r\nHost: localhost\r\n\r\n";

fn fifty_headers() -> String {
    let mut request = String::from("GET /plaintext HTTP/1.1\r\nHost: localhost\r\n");
    for i in 1..50 {
        request.push_str(&format!("X-Benchmark-Header-{i}: value-{i}-abcdefghijklmnopqrstuvwxyz\r\n"));
    }
    request.push_str("\r\n");
    request
}

fn parser(c: &mut Criterion) {
    let fifty = fifty_headers();
    let corpus = [("small", SMALL), ("browser", BROWSER), ("50_headers", fifty.as_str()), ("percent_encoded", PERCENT_ENCODED)];

    let mut group = c.benchmark_group("parser");
    for (name, request) in corpus {
        group.throughput(Throughput::Bytes(request.len() as u64));

        assert!(HttpRequest::new(request).is_ok(), "builder rejected {name}");
        group.bench_with_input(BenchmarkId::new("builder", name), request, |b, request| {
            b.iter(|| black_box(HttpRequest::new(black_box(request))));
        });

        // httparse only splits the request into pieces, it leaves validating the semantics to the caller,
        // so this is the floor rather than an equal
        group.bench_with_input(BenchmarkId::new("httparse", name), request.as_bytes(), |b, request| {
            b.iter(|| {
                let mut headers = [httparse::EMPTY_HEADER; 64];
                let mut parsed = httparse::Request::new(&mut headers);
                black_box(parsed.parse(black_box(request)).expect("httparse"));
                black_box(parsed.path);
            });
        });
    }
    group.finish();
}

// ----- Router -----

type MatchitHandler = for<'s> fn(&'s HttpResponse<'static>) -> HttpResponse<'s>;

fn builder_handler<'s>(_request: &HttpRequest<'_>, cached: &'s HttpResponse<'static>) -> HttpResponse<'s> {
    cached.borrowed()
}

fn matchit_handler<'s>(cached: &'s HttpResponse<'static>) -> HttpResponse<'s> {
    cached.borrowed()
}

// routes paths of depth segments that share all but their last segment, like a typical API
fn route_paths(routes: usize, depth: usize) -> Vec<String> {
    let prefix: String = (1..depth).map(|segment| format!("/s{segment}")).collect();
    (0..routes).map(|route| format!("{prefix}/r{route}")).collect()
}

// A fixed shuffle, so every run and both routers hit the routes in the same order, without predictable branches
fn shuffled<T>(mut items: Vec<T>) -> Vec<T> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        items.swap(i, (state % (i as u64 + 1)) as usize);
    }
    items
}

fn bench_router(c: &mut Criterion, group_name: &str, cases: &[(String, usize, usize)]) {
    let mut group = c.benchmark_group(group_name);
    for (label, routes, depth) in cases {
        let paths = route_paths(*routes, *depth);

        // Builder's lookup isn't public, so this goes through invoke, which also does its version, method, and
        // transfer-encoding checks and reads the status code for its log line
        let cached = HttpResponseBuilder::from_text(Status::Ok, "ok").build();
        let mut server = Server::new(cached.clone());
        for path in &paths {
            assert!(server.add_endpoint(Method::Get, path, builder_handler));
        }
        let raw: Vec<String> = paths.iter().map(|path| format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n")).collect();
        let requests: Vec<HttpRequest> = shuffled(raw.iter().map(|raw| HttpRequest::new(raw).expect("parse")).collect());
        let address = IpAddress::from([127, 0, 0, 1]);
        for request in &requests {
            assert_eq!(server.invoke(request, &address).status_code(), 200);
        }

        group.bench_function(BenchmarkId::new("builder", label), |b| {
            let mut next = 0;
            b.iter(|| {
                let request = &requests[next];
                next = (next + 1) % requests.len();
                black_box(server.invoke(black_box(request), &address));
            });
        });

        // A method array and a handler call on top of the lookup, to do the same work invoke does to dispatch
        let mut router = matchit::Router::new();
        for path in &paths {
            let mut handlers: [Option<MatchitHandler>; Method::COUNT] = [None; Method::COUNT];
            handlers[Method::Get as usize] = Some(matchit_handler);
            router.insert(path.as_str(), handlers).expect("insert route");
        }
        let lookups = shuffled(paths.clone());

        group.bench_function(BenchmarkId::new("matchit", label), |b| {
            let mut next = 0;
            b.iter(|| {
                let path = lookups[next].as_str();
                next = (next + 1) % lookups.len();
                let found = router.at(black_box(path)).expect("route");
                let handler = found.value[black_box(Method::Get) as usize].expect("handler");
                black_box(handler(&cached));
            });
        });
    }
    group.finish();
}

fn router_routes(c: &mut Criterion) {
    let cases: Vec<_> = [1, 10, 100, 1000].map(|routes| (format!("{routes}_routes"), routes, 2)).to_vec();
    bench_router(c, "router_by_route_count", &cases);
}

fn router_depth(c: &mut Criterion) {
    let cases: Vec<_> = [1, 4, 8].map(|depth| (format!("depth_{depth}"), 100, depth)).to_vec();
    bench_router(c, "router_by_depth", &cases);
}

criterion_group!(benches, parser, router_routes, router_depth);
criterion_main!(benches);
