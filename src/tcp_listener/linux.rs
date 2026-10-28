use std::mem;
use std::net::Ipv4Addr;

use libc::{c_int, c_void, sockaddr, sockaddr_in, socklen_t};

use super::{NativeSocket, TcpListener};
use crate::ip_address::IpAddress;

#[allow(dead_code)]
const INVALID_SOCKET: NativeSocket = -1;

fn perror(s: &std::ffi::CStr) {
    unsafe { libc::perror(s.as_ptr()) };
}

// Reads from conn one byte at a time until a newline (kept) or until buf is full
// Returns the number of bytes read, or <= 0 if the connection closed or errored before anything arrived
fn recv_line(conn: NativeSocket, buf: &mut [u8]) -> isize {
    let mut len = 0;
    while len < buf.len() {
        let n = unsafe { libc::recv(conn, buf[len..].as_mut_ptr() as *mut c_void, 1, 0) };
        if n <= 0 {
            return if len > 0 { len as isize } else { n };
        }

        len += 1;
        if buf[len - 1] == b'\n' {
            break;
        }
    }
    len as isize
}

// Sends all of data, retrying on partial sends
fn send_all(conn: NativeSocket, data: &[u8]) -> bool {
    let mut sent = 0;
    while sent < data.len() {
        let n = unsafe { libc::send(conn, data[sent..].as_ptr() as *const c_void, data.len() - sent, 0) };
        if n <= 0 {
            perror(c"send");
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
            perror(c"socket");
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
            perror(c"bind");
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
            perror(c"listen");
            return false;
        }
        println!("Now serving on port {}", self.port);

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
                perror(c"accept");
                // If there is an error here do not shut down the server just continue
                continue;
            }

            let ip = Ipv4Addr::from(u32::from_be(client.sin_addr.s_addr));

            println!("Connection from {}:{}", ip, u16::from_be(client.sin_port));

            let mut buf = [0u8; 256];
            let n = recv_line(conn, &mut buf);
            if n > 0 {
                let line = &buf[..n as usize];
                let end = line.iter().position(|&b| b == b'\r' || b == b'\n').unwrap_or(line.len());
                let request = String::from_utf8_lossy(&line[..end]);
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
