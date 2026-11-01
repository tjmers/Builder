use std::borrow::Cow;

use crate::ip_address::IpAddress;

#[cfg(windows)]
pub type NativeSocket = windows_sys::Win32::Networking::WinSock::SOCKET;

#[cfg(target_os = "linux")]
pub type NativeSocket = libc::c_int;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("OS Not supported");

// Platform implementations of new, bind, valid, run, and drop, plus the raw recv and send calls
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

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

const RECV_CHUNK_BYTES: usize = 4096;

// Requests over these limits are dropped
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

// Finds the end of the headers (just past the blank line), accepting CRLF or bare LF line endings
fn header_end(buf: &[u8]) -> Option<usize> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

// The value of the Content-Length header, or 0 if there isn't a valid one
fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .skip(1)
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("Content-Length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

// Appends the next chunk received on conn to buf
// Returns false if the connection closed or errored
fn recv_chunk(conn: NativeSocket, chunk: &mut [u8], buf: &mut Vec<u8>) -> bool {
    let n = match platform::recv(conn, chunk) {
        Ok(n) => n,
        Err(e) => {
            log_error!("recv: {}", e);
            return false;
        }
    };
    if n == 0 {
        if buf.is_empty() {
            log_error!("Connection closed before a request was received");
        } else {
            log_error!("Connection closed after {} bytes, before the full request was received", buf.len());
        }
        return false;
    }
    buf.extend_from_slice(&chunk[..n]);
    true
}

// Reads one full request from conn: the request line and headers, then Content-Length bytes of body
// Returns None if the connection closed or errored first, or if the request is too large
fn recv_request(conn: NativeSocket) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; RECV_CHUNK_BYTES];

    let header_len = loop {
        if let Some(end) = header_end(&buf) {
            break end;
        }
        if buf.len() > MAX_HEADER_BYTES {
            log_error!("Request headers exceed {} bytes", MAX_HEADER_BYTES);
            return None;
        }
        if !recv_chunk(conn, &mut chunk, &mut buf) {
            return None;
        }
    };

    let body_len = content_length(&buf[..header_len]);
    if body_len > MAX_BODY_BYTES {
        log_error!("Request body of {} bytes exceeds {} bytes", body_len, MAX_BODY_BYTES);
        return None;
    }

    let total = header_len + body_len;
    while buf.len() < total {
        if !recv_chunk(conn, &mut chunk, &mut buf) {
            return None;
        }
    }
    // Anything past the body (e.g. a pipelined request) is discarded since the connection is closed after responding
    buf.truncate(total);
    Some(buf)
}

// Sends all of data, retrying on partial sends
fn send_all(conn: NativeSocket, data: &[u8]) -> bool {
    let mut sent = 0;
    while sent < data.len() {
        match platform::send(conn, &data[sent..]) {
            Ok(n) if n > 0 => sent += n,
            Ok(_) => {
                log_error!("send: no bytes were sent");
                return false;
            }
            Err(e) => {
                log_error!("send: {}", e);
                return false;
            }
        }
    }
    true
}

impl<C> TcpListener<'_, C> {
    // Reads one request from an accepted connection and sends back the handler's response
    // The caller is responsible for closing conn
    fn serve(&self, conn: NativeSocket, ip: IpAddress) {
        if let Some(request) = recv_request(conn) {
            // This is a request -- call the request handler
            let response = (self.handler)(self.context, &request, ip);

            // Send back the response
            // The only copy is the one send makes into the kernel's socket buffer
            send_all(conn, &response);
        }
    }
}
