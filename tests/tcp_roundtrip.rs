// End to end tests that send real requests over a socket to Server::run

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::server::Server;

// Starts a server on port in the background
// The thread is never joined, it ends when the test process exits
fn start_server(port: i32) {
    thread::spawn(move || {
        let mut s = Server::new(());
        s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, "index").build()
        });
        s.add_endpoint(Method::Post, "/echo", |request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, request.content()).build()
        });
        s.add_endpoint(Method::Post, "/hex", |request: &HttpRequest, _state: &()| -> HttpResponse {
            let hex: String = request.body().iter().map(|b| format!("{b:02x}")).collect();
            HttpResponseBuilder::from_text(Status::Ok, &hex).build()
        });
        s.run(port);
    });
}

// Sends raw to the server and returns the full response, retrying until the server is listening
fn send(port: i32, raw: &[u8]) -> String {
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port as u16)) {
            stream = Some(s);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("server never started listening");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    stream.write_all(raw).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn get_returns_200() {
    let port = 38101;
    start_server(port);
    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(response.ends_with("\r\n\r\nindex"), "response: {response:?}");
}

#[test]
fn unknown_path_returns_404() {
    let port = 38102;
    start_server(port);
    let response = send(port, b"GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"), "response: {response:?}");
}

#[test]
fn body_is_read_using_content_length() {
    let port = 38103;
    start_server(port);
    let response = send(port, b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(response.ends_with("\r\n\r\nhello"), "response: {response:?}");
}

#[test]
fn request_split_across_writes_is_reassembled() {
    let port = 38104;
    start_server(port);
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port as u16)) {
            stream = Some(s);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("server never started listening");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    for part in ["POST /echo HTTP/1.1\r\nHost: loc", "alhost\r\nContent-Length: 11\r\n", "\r\nhello", " world"] {
        stream.write_all(part.as_bytes()).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(response.ends_with("\r\n\r\nhello world"), "response: {response:?}");
}

// Browsers open spare "preconnect" sockets that may never send anything
// One of those sitting idle must not stop other connections from being served
#[test]
fn idle_connection_does_not_block_other_requests() {
    let port = 38105;
    start_server(port);
    // Warm up so the server is listening before the idle socket connects
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let _idle = TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    // On linux TCP_DEFER_ACCEPT holds a silent socket back from accept for about a second, so wait until a worker has it
    thread::sleep(Duration::from_secs(2));

    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
}

#[test]
fn response_says_connection_is_closed() {
    let port = 38106;
    start_server(port);
    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.contains("\r\nConnection: close\r\n"), "response: {response:?}");
}

// Connections are served by a fixed pool of workers, so idle sockets must not each take one up
// Opening more of them than there are cores would stall the server if they did
#[test]
fn more_idle_connections_than_cores_do_not_block_other_requests() {
    let port = 38107;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let cores = thread::available_parallelism().map_or(4, |n| n.get());
    // Part of a request gets each socket accepted straight away, past TCP_DEFER_ACCEPT on linux, and then leaves it
    // open waiting for the rest
    let _idle: Vec<TcpStream> = (0..cores * 4)
        .map(|_| {
            let mut stream = TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
            stream.write_all(b"GET /index HTTP/1.1\r\n").unwrap();
            stream
        })
        .collect();
    thread::sleep(Duration::from_millis(50));

    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
}

// A handler that panics must only lose its own connection, since workers serve many at once
// Panicking on more connections than there are workers would leave nothing to serve the last request if it didn't
#[test]
fn panicking_handler_does_not_stop_the_server() {
    let port = 38110;
    thread::spawn(move || {
        let mut s = Server::new(());
        s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, "index").build()
        });
        s.add_endpoint(Method::Get, "/panic", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            panic!("this handler always panics");
        });
        s.run(port);
    });
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let cores = thread::available_parallelism().map_or(4, |n| n.get());
    for _ in 0..cores * 4 {
        let mut stream = TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(b"GET /panic HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        // Closed without a response, either cleanly or with a reset
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        assert!(response.is_empty(), "response: {:?}", String::from_utf8_lossy(&response));
    }

    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
}

// A request too big for one receive and a response too big for the socket's send buffer both take several trips
// around the event loop, and every byte still has to arrive in order
#[test]
fn large_request_and_response_arrive_whole() {
    let port = 38111;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let body: String = (0..512 * 1024).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let mut request = format!("POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    request.extend_from_slice(body.as_bytes());

    let mut stream = TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(&request).unwrap();
    // Leave the response sitting in the socket buffers for a moment, so the server's send has to wait on the client
    thread::sleep(Duration::from_millis(100));
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response starts: {:?}", &response[..response.len().min(64)]);
    let (_, echoed) = response.split_once("\r\n\r\n").expect("response has no end of headers");
    assert!(echoed == body, "the echoed body differs, {} bytes instead of {}", echoed.len(), body.len());
}

// A connection that never sends a request is eventually closed by the server
#[test]
fn idle_connection_is_closed_after_timeout() {
    let port = 38108;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut idle = TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    // The server allows 5 seconds of idleness, checked about once a second
    idle.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 16];
    let n = idle.read(&mut buf).expect("server did not close the idle connection");
    assert_eq!(n, 0, "expected the server to close the connection without a response");
}

// Workers share the port through SO_REUSEPORT, which must not let a second server quietly join one already running
#[test]
fn second_server_cannot_bind_a_port_in_use() {
    let port = 38109;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    fn handler<'a>(_: &'a (), _: &[u8], _: builder::ip_address::IpAddress) -> std::borrow::Cow<'a, [u8]> {
        std::borrow::Cow::Borrowed(b"")
    }
    let mut second = builder::tcp_listener::TcpListener::new(&(), handler);
    assert!(!second.bind(port), "a second listener bound a port the server is using");
}

// Connects to a server that may still be starting up
fn connect(port: i32) -> TcpStream {
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port as u16)) {
            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            return stream;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("server never started listening on port {port}");
}

// Two servers can both bind before either listens, and only listening settles which of them gets the port
#[cfg(target_os = "linux")]
#[test]
fn second_of_two_servers_bound_together_fails_to_run() {
    use builder::tcp_listener::TcpListener;
    use std::sync::mpsc;

    fn handler<'a>(_: &'a (), _: &[u8], _: builder::ip_address::IpAddress) -> std::borrow::Cow<'a, [u8]> {
        std::borrow::Cow::Borrowed(b"")
    }

    let port = 38112;
    let mut first = TcpListener::new(&(), handler);
    let mut second = TcpListener::new(&(), handler);
    assert!(first.bind(port), "the first listener could not bind");
    assert!(second.bind(port), "both listeners should be able to bind before either listens");

    thread::spawn(move || first.run());
    drop(connect(port));

    let (result, finished) = mpsc::channel();
    thread::spawn(move || result.send(second.run()));
    let ran = finished.recv_timeout(Duration::from_secs(2));
    assert_eq!(ran, Ok(false), "the second server should fail straight away instead of sharing the port");
}

// A client using Nagle's algorithm (the default) holds its second write back until the first is acknowledged, so the
// server must acknowledge it promptly instead of waiting about 40ms for a response to carry the ACK
#[cfg(target_os = "linux")]
#[test]
fn request_sent_in_two_writes_is_not_held_up_by_a_delayed_ack() {
    let port = 38113;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let mut times: Vec<Duration> = (0..21)
        .map(|_| {
            let mut stream = connect(port);
            let start = Instant::now();
            stream.write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\n").unwrap();
            stream.write_all(b"hello").unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            assert!(response.ends_with(b"\r\n\r\nhello"), "response: {:?}", String::from_utf8_lossy(&response));
            start.elapsed()
        })
        .collect();
    times.sort();
    let median = times[times.len() / 2];
    assert!(median < Duration::from_millis(20), "the median request took {median:?}");
}

// A slow handler only takes up its own thread, so a connection that was already open must still be served
// The server is pinned to one CPU, which is the hardest case: it gets the fewest workers the listener ever runs
#[cfg(target_os = "linux")]
#[test]
fn slow_handlers_do_not_hold_up_an_open_connection() {
    let port = 38114;
    thread::spawn(move || {
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(libc::sched_getcpu() as usize, &mut set);
            assert_eq!(libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set), 0);
        }
        let mut s = Server::new(());
        s.add_endpoint(Method::Post, "/echo", |request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, request.content()).build()
        });
        s.add_endpoint(Method::Get, "/slow", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            thread::sleep(Duration::from_secs(1));
            HttpResponseBuilder::from_text(Status::Ok, "slow").build()
        });
        s.run(port);
    });

    // Half a request gets this connection accepted, and it then waits for the rest
    let mut open = connect(port);
    open.write_all(b"POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\n").unwrap();
    thread::sleep(Duration::from_millis(100));

    let slow: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(move || {
                let mut stream = connect(port);
                stream.write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                assert!(response.ends_with("\r\n\r\nslow"), "response: {response:?}");
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    open.write_all(b"hello").unwrap();
    let mut response = String::new();
    open.read_to_string(&mut response).unwrap();
    let waited = start.elapsed();
    assert!(response.ends_with("\r\n\r\nhello"), "response: {response:?}");
    assert!(waited < Duration::from_millis(500), "the open connection waited {waited:?} behind the slow handlers");

    for client in slow {
        client.join().unwrap();
    }
}

// A client that stops reading for longer than the receive idle timeout has not necessarily gone away
#[cfg(target_os = "linux")]
#[test]
fn large_response_survives_a_client_pause() {
    const BODY_BYTES: usize = 64 * 1024 * 1024;
    let port = 38115;
    thread::spawn(move || {
        let mut s = Server::new(());
        s.add_endpoint(Method::Get, "/large", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, "a".repeat(BODY_BYTES)).build()
        });
        s.run(port);
    });

    let mut stream = connect(port);
    stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    // Longer than the 5 second idle timeout plus its 1 second sweep
    thread::sleep(Duration::from_secs(7));
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let body_start = response.windows(4).position(|w| w == b"\r\n\r\n").expect("response has no end of headers") + 4;
    assert_eq!(response.len() - body_start, BODY_BYTES);
}

// Headers past the limit are refused even when they all arrive in the first receive
#[test]
fn oversized_headers_are_dropped() {
    let port = 38116;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let request = format!("GET /index HTTP/1.1\r\nHost: localhost\r\nX-Padding: {}\r\n\r\n", "a".repeat(12 * 1024));
    let mut stream = connect(port);
    stream.write_all(request.as_bytes()).unwrap();
    // Closed without a response, either cleanly or with a reset
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    assert!(response.is_empty(), "response: {:?}", String::from_utf8_lossy(&response));
}

#[test]
fn chunked_body_is_received_and_decoded() {
    let port = 38117;
    start_server(port);
    let response = send(
        port,
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(response.ends_with("\r\n\r\nhello world"), "response: {response:?}");
}

#[test]
fn chunked_body_split_across_writes_is_reassembled() {
    let port = 38118;
    start_server(port);
    let mut stream = connect(port);
    for part in [
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n",
        "5\r\nhel",
        "lo\r\n6\r",
        "\n world\r\n0\r\n",
        "\r\n",
    ] {
        stream.write_all(part.as_bytes()).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(response.ends_with("\r\n\r\nhello world"), "response: {response:?}");
}

// Malformed chunks must get an answer rather than leaving the connection waiting for bytes that will never make sense
#[test]
fn malformed_chunked_body_gets_400() {
    let port = 38119;
    start_server(port);
    let response = send(port, b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\nnot hex\r\nhello\r\n");
    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"), "response: {response:?}");
}

#[test]
fn non_utf8_body_is_available_byte_for_byte() {
    let port = 38120;
    start_server(port);

    let response = send(port, b"POST /hex HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n\xff\x00\xfe\x80");
    assert!(response.ends_with("\r\n\r\nff00fe80"), "response: {response:?}");

    let response = send(
        port,
        b"POST /hex HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n\xff\x00\r\n2\r\n\xfe\x80\r\n0\r\n\r\n",
    );
    assert!(response.ends_with("\r\n\r\nff00fe80"), "response: {response:?}");
}
