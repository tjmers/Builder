// End to end tests that send real requests over a socket to Server::run

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

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
    thread::sleep(Duration::from_millis(50));

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
    let _idle: Vec<TcpStream> = (0..cores * 4)
        .map(|_| TcpStream::connect(("127.0.0.1", port as u16)).unwrap())
        .collect();
    thread::sleep(Duration::from_millis(50));

    let response = send(port, b"GET /index HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "response: {response:?}");
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
