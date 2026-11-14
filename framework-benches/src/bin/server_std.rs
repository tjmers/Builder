// The floor: a minimal blocking HTTP responder on std::net with a thread per connection
// Always closes after one response, which every benchmark that uses it asks for anyway

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;

use framework_benches::{port_arg, std_listener, JSON_MESSAGE, PLAINTEXT, POST_BODY, POST_BODY_BYTES};

const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

fn prebuilt(body: &[u8]) -> &'static [u8] {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response.leak()
}

// Reads one whole request (headers, then Content-Length bytes of body), and returns it
fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(2 * POST_BODY_BYTES);
    let mut chunk = [0u8; 8192];
    let headers_end = loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
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
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    Some(buf)
}

fn serve(mut stream: TcpStream, plaintext: &[u8], post: &[u8]) {
    let Some(request) = read_request(&mut stream) else { return };

    let _ = if request.starts_with(b"GET /plaintext ") {
        stream.write_all(plaintext)
    } else if request.starts_with(b"POST /post ") {
        stream.write_all(post)
    } else if request.starts_with(b"GET /json ") {
        let body = serde_json::to_vec(&JSON_MESSAGE).expect("serialize");
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        stream.write_all(&response)
    } else {
        stream.write_all(NOT_FOUND)
    };
}

fn main() {
    let plaintext = prebuilt(PLAINTEXT.as_bytes());
    let post = prebuilt(&POST_BODY);

    let listener = std_listener(port_arg());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        thread::spawn(move || serve(stream, plaintext, post));
    }
}
