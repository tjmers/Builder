// End to end tests that send real requests over a socket to Server::run

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, Status};
use builder::server::Server;

// Starts a server on port in the background
// The thread is never joined, it ends when the test process exits
fn start_server(port: i32) {
    thread::spawn(move || {
        let mut s = Server::new(());
        s.add_endpoint(Method::Get, "/index", |_request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponse::new(Status::Ok, "index")
        });
        s.add_endpoint(Method::Post, "/echo", |request: &HttpRequest, _state: &()| -> HttpResponse {
            HttpResponse::new(Status::Ok, request.content())
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
