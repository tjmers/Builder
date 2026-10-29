use std::io;
use std::mem;
use std::net::Ipv4Addr;

use libc::{c_int, c_void, sockaddr, sockaddr_in, socklen_t};

use super::{NativeSocket, TcpListener};
use crate::ip_address::IpAddress;

#[allow(dead_code)]
const INVALID_SOCKET: NativeSocket = -1;

// Logs the last OS error, prefixed by the call that failed
fn log_os_error(call: &str) {
    log_error!("{}: {}", call, io::Error::last_os_error());
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
    let n = unsafe { libc::recv(conn, chunk.as_mut_ptr() as *mut c_void, chunk.len(), 0) };
    if n < 0 {
        log_os_error("recv");
        return false;
    }
    if n == 0 {
        if buf.is_empty() {
            log_error!("Connection closed before a request was received");
        } else {
            log_error!("Connection closed after {} bytes, before the full request was received", buf.len());
        }
        return false;
    }
    buf.extend_from_slice(&chunk[..n as usize]);
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
        // MSG_NOSIGNAL so a client that already hung up gives an error instead of a SIGPIPE that kills the server
        let n = unsafe {
            libc::send(conn, data[sent..].as_ptr() as *const c_void, data.len() - sent, libc::MSG_NOSIGNAL)
        };
        if n <= 0 {
            log_os_error("send");
            return false;
        }
        sent += n as usize;
    }
    true
}

impl<'a> TcpListener<'a> {
    pub fn new(handler: impl Fn(&str, IpAddress) -> String + 'a) -> Self {
        // Create the socket but do not start listening
        let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        if socket < 0 {
            log_os_error("socket");
        }

        TcpListener {
            socket,
            handler: Box::new(handler),
            port: -1,
        }
    }

    pub fn bind(&mut self, port: i32) -> bool {
        if self.socket < 0 {
            return false;
        }
        let opt: c_int = 1;
        unsafe {
            libc::setsockopt(
                self.socket,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &opt as *const c_int as *const c_void,
                mem::size_of::<c_int>() as socklen_t,
            );
        }

        let mut addr: sockaddr_in = unsafe { mem::zeroed() };
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
        addr.sin_port = (port as u16).to_be();

        // Bind
        let bind_status = unsafe {
            libc::bind(
                self.socket,
                &addr as *const sockaddr_in as *const sockaddr,
                mem::size_of::<sockaddr_in>() as socklen_t,
            )
        };
        if bind_status < 0 {
            log_os_error("bind");
            self.socket = bind_status;
            return false;
        }
        self.port = port;
        true
    }

    pub fn valid(&self) -> bool {
        self.socket >= 0
    }

    pub fn run(&self) -> bool {
        // Start listening
        if self.socket < 0 || self.port < 0 {
            return false;
        }
        let listen_status = unsafe { libc::listen(self.socket, libc::SOMAXCONN) };
        if listen_status < 0 {
            log_os_error("listen");
            return false;
        }
        log_info!("Now serving on port {}", self.port);

        loop {
            let mut client: sockaddr_in = unsafe { mem::zeroed() };
            let mut client_len = mem::size_of::<sockaddr_in>() as socklen_t;
            let conn = unsafe {
                libc::accept(
                    self.socket,
                    &mut client as *mut sockaddr_in as *mut sockaddr,
                    &mut client_len,
                )
            };
            if conn < 0 {
                log_os_error("accept");
                // If there is an error here do not shut down the server just continue
                continue;
            }

            let ip = Ipv4Addr::from(u32::from_be(client.sin_addr.s_addr));

            log_info!("Connection from {}:{}", ip, u16::from_be(client.sin_port));

            if let Some(request) = recv_request(conn) {
                let request = String::from_utf8_lossy(&request);
                // This is a request -- call the request handler
                let response = (self.handler)(&request, IpAddress::from(ip.octets()));

                // Send back the response
                send_all(conn, response.as_bytes());
            }

            unsafe { libc::close(conn) };
        }
    }
}

impl Drop for TcpListener<'_> {
    fn drop(&mut self) {
        if self.socket >= 0 {
            unsafe { libc::close(self.socket) };
        }
    }
}
