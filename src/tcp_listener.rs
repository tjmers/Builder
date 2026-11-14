use std::borrow::Cow;
use std::time::Duration;

use crate::http_request::{scan_head, BodyKind, ChunkError, ChunkWalker};
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

// A connection with a response left to send is closed after this long without the client taking any of it
// Longer than IDLE_TIMEOUT since a client that pauses partway through a download has not necessarily gone away, but
// still bounded so a client that stops reading cannot hold on to the response forever
pub(crate) const SEND_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

// Requests over these limits are dropped
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

// How far along the request in a connection's receive buffer is
pub(crate) enum Framing {
    // More bytes are needed
    Incomplete,
    // The request is the first this many bytes (anything after, like a pipelined request, is not part of it)
    Complete(usize),
    // The request is over the size limits and the connection should be dropped
    TooLarge,
}

// What a Framer knows about its request so far
#[derive(Default)]
enum Stage {
    // The head has not all arrived
    #[default]
    Head,
    // The head has been read, and the request ends this many bytes in
    Length(usize),
    // The head, of header_len bytes, has been read and is followed by chunks
    Chunked { header_len: usize, chunks: ChunkWalker },
}

// Frames the one request a connection sends: the request line and headers, then a body of Content-Length bytes or
// chunks
// It keeps its place between receives, so the head is read once and each chunk once, however many pieces the
// request arrives in
// Shared by the Windows and linux listeners so both apply the same limits, and it reads the head with the parser's
// own scan (scan_head), so the two cannot disagree about where a request ends or whether it is valid
#[derive(Default)]
pub(crate) struct Framer {
    stage: Stage,
}

impl Framer {
    // buf is everything received so far, which only ever grows between calls
    pub(crate) fn advance(&mut self, buf: &[u8]) -> Framing {
        if let Stage::Head = self.stage {
            let (header_len, body) = match scan_head(buf) {
                Some((header_len, body)) if header_len <= MAX_HEADER_BYTES => (header_len, body),
                None if buf.len() <= MAX_HEADER_BYTES => return Framing::Incomplete,
                _ => {
                    log_error!("Request headers exceed {} bytes", MAX_HEADER_BYTES);
                    return Framing::TooLarge;
                }
            };
            self.stage = match body {
                Ok(BodyKind::Length(body_len)) if body_len > MAX_BODY_BYTES => {
                    log_error!("Request body of {} bytes exceeds {} bytes", body_len, MAX_BODY_BYTES);
                    return Framing::TooLarge;
                }
                Ok(BodyKind::Length(body_len)) => Stage::Length(header_len + body_len),
                Ok(BodyKind::Chunked) => Stage::Chunked { header_len, chunks: ChunkWalker::default() },
                // The server answers another transfer coding with 501 without reading a body
                Ok(BodyKind::None | BodyKind::OtherCoding) => Stage::Length(header_len),
                // The parser is going to reject these headers (e.g. Transfer-Encoding alongside Content-Length), so
                // hand everything over for it to answer with a 400, rather than waiting for a body that cannot be
                // trusted to end where the headers say
                Err(_) => return Framing::Complete(buf.len()),
            };
        }

        match &mut self.stage {
            Stage::Head => Framing::Incomplete,
            Stage::Length(total) if buf.len() < *total => Framing::Incomplete,
            Stage::Length(total) => Framing::Complete(*total),
            Stage::Chunked { header_len, chunks } => {
                let header_len = *header_len;
                match chunks.advance(&buf[header_len..], |_| {}) {
                    Ok(body_len) if body_len <= MAX_BODY_BYTES => Framing::Complete(header_len + body_len),
                    // Hand everything over so the parser answers it with a 400, rather than waiting for bytes that will not help
                    Err(ChunkError::Malformed) => Framing::Complete(buf.len()),
                    Err(ChunkError::Incomplete) if buf.len() <= header_len + MAX_BODY_BYTES => Framing::Incomplete,
                    // The limit applies to the body with its framing, which is all that is known before the end has arrived
                    _ => {
                        log_error!("Chunked request body exceeds {} bytes", MAX_BODY_BYTES);
                        Framing::TooLarge
                    }
                }
            }
        }
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

    // Frames a request that has arrived all at once
    fn request_len(buf: &[u8]) -> Framing {
        Framer::default().advance(buf)
    }

    #[test]
    fn request_len_finds_crlf_and_bare_lf_blank_lines() {
        assert!(matches!(request_len(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nbody"), Framing::Complete(27)));
        assert!(matches!(request_len(b"GET / HTTP/1.1\nHost: a\n\nbody"), Framing::Complete(24)));
        assert!(matches!(request_len(b"GET / HTTP/1.1\r\nHost: a\r\n"), Framing::Incomplete));
        assert!(matches!(request_len(b""), Framing::Incomplete));
    }

    #[test]
    fn request_len_ends_the_head_where_the_parser_does() {
        // The parser takes a line ending in LF or CRLF, in any mix, so an LF line followed by a CRLF blank line ends the head
        assert!(matches!(request_len(b"GET / HTTP/1.1\nHost: a\n\r\nbody"), Framing::Complete(25)));
        // It also skips empty lines before the request line, so they do not end the head
        assert!(matches!(request_len(b"\r\n\r\n"), Framing::Incomplete));
        assert!(matches!(request_len(b"\r\n\r\nGET / HTTP/1.1\r\nHost: a\r\n\r\n"), Framing::Complete(31)));
    }

    #[test]
    fn request_len_stops_at_the_first_blank_line() {
        // A blank line inside the body must not move the end of the headers
        assert!(matches!(request_len(b"POST / HTTP/1.1\r\n\r\nline\n\nmore\r\n\r\n"), Framing::Complete(19)));
    }

    #[test]
    fn request_len_hands_headers_the_parser_rejects_straight_to_it() {
        // No body is waited for, since the parser answers these with a 400 whatever follows
        let both = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\nhello";
        assert!(matches!(request_len(both), Framing::Complete(n) if n == both.len()));

        let conflicting = b"POST / HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 50\r\n\r\n";
        assert!(matches!(request_len(conflicting), Framing::Complete(n) if n == conflicting.len()));

        let not_a_number = b"POST / HTTP/1.1\r\nContent-Length: +5\r\n\r\n";
        assert!(matches!(request_len(not_a_number), Framing::Complete(n) if n == not_a_number.len()));
    }

    // Feeds request to one Framer in pieces of at most step bytes, as it would arrive over several receives
    // Returns the framing once it is no longer incomplete, and how many bytes had arrived by then
    fn frame_in_pieces(request: &[u8], step: usize) -> (Framing, usize) {
        let mut framer = Framer::default();
        let mut len = 0;
        loop {
            len = (len + step).min(request.len());
            match framer.advance(&request[..len]) {
                Framing::Incomplete if len < request.len() => {}
                framing => return (framing, len),
            }
        }
    }

    #[test]
    fn framer_keeps_its_place_between_receives() {
        let requests: [&[u8]; 3] = [
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nTrailer: x\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello",
            b"\r\nGET / HTTP/1.1\nHost: a\n\r\n",
        ];
        for request in requests {
            for step in [1, 2, 3, 7] {
                // Complete exactly when the last byte arrives, and not before
                let (framing, arrived) = frame_in_pieces(request, step);
                assert!(matches!(framing, Framing::Complete(n) if n == request.len()), "in pieces of {step}");
                assert_eq!(arrived, request.len(), "in pieces of {step}");
            }
        }
    }

    // Walking the chunks again from the first one after every receive would take time that grows with the square of
    // the body, which let one client sending tiny chunks in tiny pieces keep a worker busy for minutes
    #[test]
    fn framer_reads_each_chunk_of_a_body_of_tiny_chunks_once() {
        let mut request = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let head_len = request.len();
        while request.len() - head_len < MAX_BODY_BYTES - 64 {
            request.extend_from_slice(b"1\r\na\r\n");
        }
        request.extend_from_slice(b"0\r\n\r\n");

        let start = std::time::Instant::now();
        let (framing, _) = frame_in_pieces(&request, 64);
        assert!(matches!(framing, Framing::Complete(n) if n == request.len()));
        // About 16,000 receives: milliseconds when each chunk is read once, and minutes when they are all read every time
        assert!(start.elapsed() < Duration::from_secs(2), "framing took {:?}", start.elapsed());
    }

    // A chunked body that stops short of a line ending, sent in small pieces, must not be searched again from the
    // start of that line on every receive
    // Each of these took seconds of CPU for one request when it was
    #[test]
    fn framer_reads_a_chunked_body_with_an_unfinished_line_once() {
        let head = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let filler = MAX_BODY_BYTES - 1024;

        // A chunk size that never ends
        let mut endless_size = head.to_vec();
        endless_size.resize(head.len() + filler, b'1');
        // A long chunk extension, complete, and then the chunk's data in small pieces
        let mut long_extension = head.to_vec();
        long_extension.extend_from_slice(format!("{:x};", filler / 2).as_bytes());
        long_extension.resize(head.len() + filler / 2, b'e');
        long_extension.extend_from_slice(b"\r\n");
        long_extension.resize(long_extension.len() + filler / 2, b'a');
        // A trailer that never ends
        let mut endless_trailer = head.to_vec();
        endless_trailer.extend_from_slice(b"0\r\n");
        endless_trailer.resize(head.len() + filler, b't');

        for (name, request) in [("size", endless_size), ("extension", long_extension), ("trailer", endless_trailer)] {
            let start = std::time::Instant::now();
            let (framing, arrived) = frame_in_pieces(&request, 16);
            assert!(matches!(framing, Framing::Incomplete), "unfinished {name}");
            assert_eq!(arrived, request.len(), "unfinished {name}");
            // About 65,000 receives: milliseconds when each byte is read once, and most of a minute when the line is
            // read again every time
            assert!(start.elapsed() < Duration::from_secs(2), "framing an unfinished {name} took {:?}", start.elapsed());
        }
    }

    #[test]
    fn framer_refuses_anything_but_a_line_ending_after_chunk_data() {
        let head = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        // Handed to the parser as soon as the byte after the data shows it is malformed, with no line ending in sight
        let mut request = head.clone();
        request.extend_from_slice(b"3\r\nabcx");
        assert!(matches!(request_len(&request), Framing::Complete(n) if n == request.len()));
        // A CR alone could still become a CRLF
        let mut request = head.clone();
        request.extend_from_slice(b"3\r\nabc\r");
        assert!(matches!(request_len(&request), Framing::Incomplete));
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
