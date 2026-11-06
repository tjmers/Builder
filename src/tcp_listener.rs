use std::borrow::Cow;
use std::time::Duration;

use crate::http_request::{walk_chunks, ChunkError};
use crate::ip_address::IpAddress;

#[cfg(windows)]
pub type NativeSocket = windows_sys::Win32::Networking::WinSock::SOCKET;

#[cfg(target_os = "linux")]
pub type NativeSocket = libc::c_int;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("OS Not supported");

// Platform implementations of new, bind, valid, run, and drop
#[cfg(target_os = "linux")]
mod linux;

#[cfg(windows)]
mod windows;

// Takes the listener's context (e.g. the server), the raw request bytes, and the client address,
// and returns the raw response bytes to send
// A cached response is returned as Cow::Borrowed from the context so it is sent straight from the cache
// without being copied, and a freshly built one as Cow::Owned
pub type RawRequestHandler<'a, C> = fn(&'a C, &[u8], IpAddress) -> Cow<'a, [u8]>;

pub struct TcpListener<'a, C> {
    socket: NativeSocket,
    handler: RawRequestHandler<'a, C>,
    context: &'a C,
    port: i32,
}

// A connection that sends nothing for this long is closed
// Browsers open spare "preconnect" sockets they may never send a request on, so an idle connection is normal
// and must not tie up the server
// Idle connections are swept periodically, so the effective limit is up to a sweep interval longer
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

// Requests over these limits are dropped
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

// Finds the end of the headers (just past the blank line), accepting CRLF or bare LF line endings
// Stops at the first blank line, so a large body after the headers is never scanned
fn header_end(buf: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = buf[from..].iter().position(|&b| b == b'\n').map(|i| i + from) {
        match &buf[i + 1..] {
            [b'\n', ..] => return Some(i + 2),
            [b'\r', b'\n', ..] if i > 0 && buf[i - 1] == b'\r' => return Some(i + 3),
            _ => from = i + 1,
        }
    }
    None
}

// The value of the first header with this name
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

// Frames a chunked body starting at header_len
fn chunked_request_len(buf: &[u8], header_len: usize) -> Framing {
    let limit = header_len + MAX_BODY_BYTES;
    match walk_chunks(&buf[header_len..], |_| {}) {
        Ok(body_len) if header_len + body_len <= limit => Framing::Complete(header_len + body_len),
        // Hand everything over so the parser answers it with a 400, rather than waiting for bytes that will not help
        Err(ChunkError::Malformed) => Framing::Complete(buf.len()),
        Err(ChunkError::Incomplete) if buf.len() <= limit => Framing::Incomplete,
        // The limit applies to the body with its framing, which is all that is known before the end has arrived
        _ => Framing::TooLarge,
    }
}

// How far along the request in a connection's receive buffer is
pub(crate) enum Framing {
    // More bytes are needed
    Incomplete,
    // The request is the first this many bytes (anything after, like a pipelined request, is not part of it)
    Complete(usize),
    // The request is over the size limits and the connection should be dropped
    TooLarge,
}

// Frames one request at the start of buf: the request line and headers, then a body of Content-Length bytes or chunks
// Shared by the Windows and linux listeners so both apply the same limits
pub(crate) fn request_len(buf: &[u8]) -> Framing {
    let header_len = match header_end(buf) {
        Some(header_len) if header_len <= MAX_HEADER_BYTES => header_len,
        None if buf.len() <= MAX_HEADER_BYTES => return Framing::Incomplete,
        _ => {
            log_error!("Request headers exceed {} bytes", MAX_HEADER_BYTES);
            return Framing::TooLarge;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_len]);

    // Only the first Transfer-Encoding counts, as in the parser, which rejects one sent alongside a Content-Length
    if header_value(&head, "Transfer-Encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
        let framing = chunked_request_len(buf, header_len);
        if matches!(framing, Framing::TooLarge) {
            log_error!("Chunked request body exceeds {} bytes", MAX_BODY_BYTES);
        }
        return framing;
    }

    let body_len = header_value(&head, "Content-Length").and_then(|value| value.parse().ok()).unwrap_or(0);
    if body_len > MAX_BODY_BYTES {
        log_error!("Request body of {} bytes exceeds {} bytes", body_len, MAX_BODY_BYTES);
        return Framing::TooLarge;
    }

    let total = header_len + body_len;
    if buf.len() < total {
        Framing::Incomplete
    } else {
        Framing::Complete(total)
    }
}

// Logs why a connection closed before its request was complete, given the bytes received so far
pub(crate) fn log_closed_early(received: usize) {
    if received == 0 {
        log_error!("Connection closed before a request was received");
    } else {
        log_error!("Connection closed after {} bytes, before the full request was received", received);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_end_finds_crlf_and_bare_lf_blank_lines() {
        assert_eq!(header_end(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nbody"), Some(27));
        assert_eq!(header_end(b"GET / HTTP/1.1\nHost: a\n\nbody"), Some(24));
        assert_eq!(header_end(b"GET / HTTP/1.1\r\nHost: a\r\n"), None);
        assert_eq!(header_end(b""), None);
    }

    #[test]
    fn header_end_needs_a_cr_before_a_crlf_blank_line() {
        // An LF followed by CRLF is not a blank line in either style
        assert_eq!(header_end(b"GET / HTTP/1.1\nHost: a\n\r\n"), None);
        assert_eq!(header_end(b"\n\r\n"), None);
    }

    #[test]
    fn header_end_stops_at_the_first_blank_line() {
        // A blank line inside the body must not move the end of the headers
        assert_eq!(header_end(b"POST / HTTP/1.1\r\n\r\nline\n\nmore\r\n\r\n"), Some(19));
    }

    #[test]
    fn request_len_frames_by_content_length() {
        let head = b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n";
        let mut buf = head.to_vec();
        assert!(matches!(request_len(&buf), Framing::Incomplete));
        buf.extend_from_slice(b"hel");
        assert!(matches!(request_len(&buf), Framing::Incomplete));
        buf.extend_from_slice(b"lo and a pipelined request");
        assert!(matches!(request_len(&buf), Framing::Complete(n) if n == head.len() + 5));
    }

    #[test]
    fn request_len_rejects_oversized_requests() {
        let mut headers = b"GET / HTTP/1.1\r\n".to_vec();
        headers.resize(MAX_HEADER_BYTES + 1, b'a');
        assert!(matches!(request_len(&headers), Framing::TooLarge));

        let body = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        assert!(matches!(request_len(body.as_bytes()), Framing::TooLarge));
    }

    // A complete request with no body whose head is exactly len bytes
    fn head_of_len(len: usize) -> Vec<u8> {
        let mut head = b"GET / HTTP/1.1\r\nX: ".to_vec();
        head.resize(len - 4, b'a');
        head.extend_from_slice(b"\r\n\r\n");
        head
    }

    #[test]
    fn request_len_applies_the_header_limit_to_a_complete_head() {
        let at_limit = head_of_len(MAX_HEADER_BYTES);
        assert!(matches!(request_len(&at_limit), Framing::Complete(n) if n == MAX_HEADER_BYTES));
        assert!(matches!(request_len(&head_of_len(MAX_HEADER_BYTES + 1)), Framing::TooLarge));
    }

    #[test]
    fn request_len_frames_chunked_bodies() {
        let request = b"POST / HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nTrailer: x\r\n\r\n";
        // Every prefix past the head needs more bytes
        for len in b"POST / HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n".len()..request.len() {
            assert!(matches!(request_len(&request[..len]), Framing::Incomplete), "prefix of {len} bytes");
        }
        assert!(matches!(request_len(request), Framing::Complete(n) if n == request.len()));

        let mut pipelined = request.to_vec();
        pipelined.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
        assert!(matches!(request_len(&pipelined), Framing::Complete(n) if n == request.len()));
    }

    #[test]
    fn request_len_hands_malformed_chunks_to_the_parser() {
        let bad_size = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nxyz\r\n";
        assert!(matches!(request_len(bad_size), Framing::Complete(n) if n == bad_size.len()));

        let bad_end = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nabc\r\n";
        assert!(matches!(request_len(bad_end), Framing::Complete(n) if n == bad_end.len()));
    }

    #[test]
    fn request_len_rejects_oversized_chunked_bodies() {
        let head = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();

        // Still arriving, and already over the limit
        let mut unfinished = head.clone();
        unfinished.extend_from_slice(format!("{:x}\r\n", MAX_BODY_BYTES * 2).as_bytes());
        unfinished.resize(head.len() + MAX_BODY_BYTES + 1, b'a');
        assert!(matches!(request_len(&unfinished), Framing::TooLarge));

        // Finished, but over the limit
        let mut finished = head.clone();
        finished.extend_from_slice(format!("{:x}\r\n", MAX_BODY_BYTES + 1).as_bytes());
        finished.resize(finished.len() + MAX_BODY_BYTES + 1, b'a');
        finished.extend_from_slice(b"\r\n0\r\n\r\n");
        assert!(matches!(request_len(&finished), Framing::TooLarge));
    }

    #[test]
    fn request_len_ignores_other_transfer_codings() {
        // The parser leaves these for the server to answer with 501, so nothing more is waited for
        let gzip = b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n";
        assert!(matches!(request_len(gzip), Framing::Complete(n) if n == gzip.len()));
    }
}
