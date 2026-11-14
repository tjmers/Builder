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
        pin_to_current_cpu();
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
#[test]
fn large_response_survives_a_client_pause() {
    const BODY_BYTES: usize = 64 * 1024 * 1024;
    let port = 38115;
    start_large_server(port, BODY_BYTES);

    let mut stream = connect(port);
    stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    // Longer than the 5 second idle timeout plus its 1 second sweep
    thread::sleep(Duration::from_secs(7));
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    assert_eq!(body_len(&response), BODY_BYTES);
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

// A header that is not UTF-8 (Latin-1 here) has its invalid bytes replaced, which makes the head longer than it was
// sent, and that must not reach the body, whose lengths count the bytes as they were sent
#[test]
fn non_utf8_header_leaves_the_body_as_it_was_sent() {
    let port = 38121;
    start_server(port);

    let response = send(
        port,
        b"POST /hex HTTP/1.1\r\nHost: localhost\r\nX-Name: caf\xe9\r\nContent-Length: 4\r\n\r\n\xff\x00\xfe\x80",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(response.ends_with("\r\n\r\nff00fe80"), "response: {response:?}");

    // A length that would end partway through a replaced byte
    let response = send(port, b"POST /hex HTTP/1.1\r\nHost: localhost\r\nX-Name: caf\xe9\r\nContent-Length: 2\r\n\r\n\xff\x00");
    assert!(response.ends_with("\r\n\r\nff00"), "response: {response:?}");

    let response = send(
        port,
        b"POST /hex HTTP/1.1\r\nHost: localhost\r\nX-Name: caf\xe9\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n\xff\x00\r\n2\r\n\xfe\x80\r\n0\r\n\r\n",
    );
    assert!(response.ends_with("\r\n\r\nff00fe80"), "response: {response:?}");
}

// The parser rejects Transfer-Encoding sent alongside Content-Length as a likely smuggling attempt, so the listener
// must hand the request over as soon as the head arrives instead of waiting for chunks that never come
#[test]
fn chunked_with_content_length_gets_400_straight_away() {
    let port = 38122;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let start = Instant::now();
    let response = send(
        port,
        b"POST /echo HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"), "response: {response:?}");
    assert!(start.elapsed() < Duration::from_secs(2), "the 400 took {:?}", start.elapsed());
}

// The parser ignores empty lines before the request line and takes LF or CRLF line endings in any mix, so the
// listener has to find the end of the head the same way
#[test]
fn head_is_framed_the_way_the_parser_reads_it() {
    let port = 38123;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    let response = send(port, b"\r\n\r\nGET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");

    let start = Instant::now();
    let response = send(port, b"GET /index HTTP/1.1\nHost: localhost\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
    assert!(start.elapsed() < Duration::from_secs(2), "the response took {:?}", start.elapsed());
}

// Closing a connection with client data still unread resets it, which must not take the response with it, so the
// server takes what the client sends until the client has the response, and the client never sees a reset
// The extra bytes stand in for anything a client sends past its request, like a pipelined request
#[test]
fn response_is_sent_before_a_close_with_unread_data() {
    let port = 38124;
    start_server(port);
    send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");

    for _ in 0..20 {
        // More than the server reads in one receive, all in one write, so some is still unread when it closes
        let mut request = b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec();
        request.resize(request.len() + 64 * 1024, b'a');

        let mut stream = connect(port);
        stream.write_all(&request).expect("the connection was reset before the server had taken the request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("the connection was reset where the response should have ended");
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
        assert!(response.ends_with("\r\n\r\nindex"), "response: {response:?}");
    }
}

// A request that has arrived but is waiting for a worker is not idle, however long the handlers ahead of it take
// The server is pinned to one CPU so it has the fewest workers the listener ever runs (five), and each batch of
// handlers takes half the idle timeout, so the last requests wait well past it
#[cfg(target_os = "linux")]
#[test]
fn requests_waiting_for_a_worker_are_not_closed_as_idle() {
    let port = 38125;
    thread::spawn(move || {
        pin_to_current_cpu();
        let mut s = Server::new(());
        s.add_endpoint(Method::Get, "/slow", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            thread::sleep(Duration::from_millis(2500));
            HttpResponseBuilder::from_text(Status::Ok, "slow").build()
        });
        s.run(port);
    });
    drop(connect(port));

    let clients: Vec<_> = (0..14)
        .map(|_| {
            thread::spawn(move || {
                let mut stream = connect(port);
                stream.write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                let mut response = Vec::new();
                let _ = stream.read_to_end(&mut response);
                String::from_utf8_lossy(&response).into_owned()
            })
        })
        .collect();
    let responses: Vec<String> = clients.into_iter().map(|client| client.join().unwrap()).collect();
    let answered = responses.iter().filter(|response| response.ends_with("\r\n\r\nslow")).count();
    assert_eq!(answered, responses.len(), "only {answered} of {} requests were answered", responses.len());
}

// The length of a response's body
fn body_len(response: &[u8]) -> usize {
    let body_start = response.windows(4).position(|w| w == b"\r\n\r\n").expect("response has no end of headers") + 4;
    response.len() - body_start
}

// Starts a server with a large response at /large
fn start_large_server(port: i32, body_bytes: usize) {
    thread::spawn(move || {
        // The size is the server's state, since a handler cannot capture anything
        let mut s = Server::new(body_bytes);
        s.add_endpoint(Method::Get, "/large", |_request: &HttpRequest, body_bytes: &usize| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, "a".repeat(*body_bytes)).build()
        });
        s.run(port);
    });
}

// A client that takes its response slowly but never stops is still there, however long the download takes
// At this rate the server's socket has no room for more for minutes at a time, so the server only sees the client's
// progress in what it acknowledges
#[test]
fn large_response_survives_a_slow_steady_reader() {
    const BODY_BYTES: usize = 64 * 1024 * 1024;
    let port = 38126;
    start_large_server(port, BODY_BYTES);

    let mut stream = connect(port);
    stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    // 16KB a second for longer than the 30 second send timeout plus its 1 second sweep
    let mut response = Vec::new();
    let mut piece = [0; 4 * 1024];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(35) {
        stream.read_exact(&mut piece).expect("the connection was closed on a client that was still reading");
        response.extend_from_slice(&piece);
        thread::sleep(Duration::from_millis(250));
    }
    stream.read_to_end(&mut response).expect("the connection was reset before the response ended");
    assert_eq!(body_len(&response), BODY_BYTES);
}

// A client that takes none of its response is closed, so it cannot hold on to the response forever
#[test]
fn client_that_stops_reading_is_closed() {
    const BODY_BYTES: usize = 64 * 1024 * 1024;
    let port = 38127;
    start_large_server(port, BODY_BYTES);

    let mut stream = connect(port);
    stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    // Longer than the 30 second send timeout plus its 1 second sweep
    thread::sleep(Duration::from_secs(33));
    // Whatever was on its way when the server closed still arrives, and the rest never does
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    assert!(response.len() < BODY_BYTES, "the whole response arrived after the client had stopped reading");
}

// Closing a connection with client data still unread resets it, and a reset discards what has not gone out yet, so
// a response too large to go out at once must not be closed until the client has it
// The extra bytes stand in for anything a client sends past its request: a line ending after it, or a pipelined request
#[test]
fn large_response_arrives_whole_with_unread_data() {
    const BODY_BYTES: usize = 8 * 1024 * 1024;
    let port = 38128;
    start_large_server(port, BODY_BYTES);

    for extra in [&b"\r\n"[..], &[b'a'; 64 * 1024]] {
        let mut stream = connect(port);
        stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        // In a later segment, so it arrives after the request has been read
        thread::sleep(Duration::from_millis(50));
        stream.write_all(extra).unwrap();

        // Slowly all the way through, so the server has handed over the last of the response while much of it is
        // still waiting to go out
        // A reset ends the reading early, as the end of the response does
        let mut response = Vec::new();
        let mut piece = [0; 64 * 1024];
        while let Ok(n @ 1..) = stream.read(&mut piece) {
            response.extend_from_slice(&piece[..n]);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(body_len(&response), BODY_BYTES, "with {} extra bytes", extra.len());
    }
}

// What a client sends while the end of its response waits to go out is discarded, and is no sign of it taking the
// response, so a client that takes none of it is still closed however much it sends
// Only on linux, where the listener keeps receiving once the response is in the kernel's buffer: on Windows a
// response this size is still being sent, and nothing is received meanwhile
#[cfg(target_os = "linux")]
#[test]
fn client_that_floods_while_its_response_lingers_is_closed() {
    // Small enough for the kernel to take all of it at once, and too large to all go out to a client that reads none
    const BODY_BYTES: usize = 1024 * 1024;
    let port = 38131;
    start_large_server(port, BODY_BYTES);

    let mut stream = connect(port);
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();

    // Without a pause, so the server's socket always has something waiting to be read
    let junk = [b'a'; 16 * 1024];
    let start = Instant::now();
    let closed = loop {
        match stream.write_all(&junk) {
            Ok(()) if start.elapsed() < Duration::from_secs(45) => {}
            Ok(()) => break false,
            // The server no longer taking what is sent is not the server closing
            Err(e) => break !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
        }
    };
    // The 30 second send timeout, and some room for a busy machine
    assert!(closed, "the connection was still open after {:?}", start.elapsed());
    assert!(start.elapsed() >= Duration::from_secs(25), "closed after only {:?}", start.elapsed());
}

// Keeps the calling thread on the CPU it is running on
// A server started from it then gets the fewest workers the listener ever runs: one pinned to that CPU, and four
// that serve parked connections
#[cfg(target_os = "linux")]
fn pin_to_current_cpu() {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(libc::sched_getcpu() as usize, &mut set);
        assert_eq!(libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set), 0);
    }
}

// Starts a server pinned to one CPU, which gets the fewest workers the listener ever runs: one pinned to that CPU,
// and four that serve parked connections
// Its /slow takes a second to answer
#[cfg(target_os = "linux")]
fn start_pinned_server(port: i32) {
    thread::spawn(move || {
        pin_to_current_cpu();
        let mut s = Server::new(());
        s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponseBuilder::from_text(Status::Ok, "index").build()
        });
        s.add_endpoint(Method::Get, "/slow", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            thread::sleep(Duration::from_secs(1));
            HttpResponseBuilder::from_text(Status::Ok, "slow").build()
        });
        s.run(port);
    });
}

// Connects to a server on this machine from a chosen local port, or None if that port is taken
// A connection is otherwise given any free port, which can be one a server in another test is about to listen on,
// and opening hundreds of them at once makes that likely
#[cfg(target_os = "linux")]
fn connect_from(local_port: u16, port: i32) -> Option<TcpStream> {
    use std::os::fd::FromRawFd;

    let address = |port: u16| {
        let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
        address.sin_port = port.to_be();
        address
    };
    let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    unsafe {
        let socket = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        assert!(socket >= 0, "socket: {}", std::io::Error::last_os_error());
        // The stream closes the socket if either call fails
        let stream = TcpStream::from_raw_fd(socket);
        let (local, remote) = (address(local_port), address(port as u16));
        let connected = libc::bind(socket, &local as *const _ as *const libc::sockaddr, len) == 0
            && libc::connect(socket, &remote as *const _ as *const libc::sockaddr, len) == 0;
        connected.then_some(stream)
    }
}

// Connections that are open but idle take up no thread, so however many there are, a request that arrives behind a
// slow handler is still handed to another worker
#[cfg(target_os = "linux")]
#[test]
fn idle_connections_do_not_stop_requests_being_taken_from_a_busy_worker() {
    // Both ends of every connection are in this process, which is more files than a process may usually have open
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = limit.rlim_max.min(8192);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
    let port = 38129;
    start_pinned_server(port);

    drop(connect(port));
    // Part of a request line gets each connection accepted, and it then waits for the rest
    let mut idle = Vec::new();
    let mut local_port = 20000;
    while idle.len() < 520 {
        if let Some(mut stream) = connect_from(local_port, port) {
            stream.write_all(b"GET /in").unwrap();
            idle.push(stream);
        }
        local_port += 1;
        assert!(local_port < 22000, "only {} connections could be opened", idle.len());
    }
    thread::sleep(Duration::from_millis(200));

    let slow = thread::spawn(move || send(port, b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n"));
    thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let waited = start.elapsed();
    assert!(response.ends_with("\r\n\r\nindex"), "response: {response:?}");
    assert!(waited < Duration::from_millis(500), "the request waited {waited:?} behind the slow handler");

    assert!(slow.join().unwrap().ends_with("\r\n\r\nslow"));
    drop(idle);
}

// Requests are only taken from a busy worker's queue when another worker is free to serve them, so the busy worker
// still gets to the rest when it comes back, instead of finding its queue empty while they wait for someone else
// With five workers, 14 one second requests take three rounds, and took four when one worker sat out each round
#[cfg(target_os = "linux")]
#[test]
fn every_worker_takes_its_share_of_queued_requests() {
    let port = 38130;
    start_pinned_server(port);
    drop(connect(port));

    let start = Instant::now();
    let clients: Vec<_> = (0..14)
        .map(|_| thread::spawn(move || send(port, b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")))
        .collect();
    for client in clients {
        assert!(client.join().unwrap().ends_with("\r\n\r\nslow"));
    }
    let took = start.elapsed();
    assert!(took < Duration::from_millis(3700), "14 requests of a second each took {took:?} on five workers");
}
