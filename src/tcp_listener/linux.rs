use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

use libc::{c_int, c_void, sockaddr, sockaddr_in, socklen_t};

use super::{NativeSocket, RawRequestHandler, TcpListener};
use crate::ip_address::IpAddress;

#[allow(dead_code)]
const INVALID_SOCKET: NativeSocket = -1;

// Logs the last OS error, prefixed by the call that failed
fn log_os_error(call: &str) {
    log_error!("{}: {}", call, io::Error::last_os_error());
}

// Receives into buf, returning the number of bytes read (0 once the peer has closed)
pub(super) fn recv(conn: NativeSocket, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::recv(conn, buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

// Sends as much of data as the OS accepts, returning the number of bytes sent
pub(super) fn send(conn: NativeSocket, data: &[u8]) -> io::Result<usize> {
    // MSG_NOSIGNAL so a client that already hung up gives an error instead of a SIGPIPE that kills the server
    let n = unsafe { libc::send(conn, data.as_ptr() as *const c_void, data.len(), libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

// Makes recv on conn fail with WouldBlock once it has waited timeout without receiving anything
pub(super) fn set_recv_timeout(conn: NativeSocket, timeout: Duration) -> io::Result<()> {
    let tv = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let status = unsafe {
        libc::setsockopt(
            conn,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const c_void,
            mem::size_of::<libc::timeval>() as socklen_t,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn close(conn: NativeSocket) {
    unsafe { libc::close(conn) };
}

impl<'a, C> TcpListener<'a, C> {
    pub fn new(context: &'a C, handler: RawRequestHandler<'a, C>) -> Self {
        // Create the socket but do not start listening
        let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        if socket < 0 {
            log_os_error("socket");
        }

        TcpListener {
            socket,
            handler,
            context,
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
}

// C must be Sync because every connection is served on its own thread, all sharing the context
impl<C: Sync> TcpListener<'_, C> {
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

        // Scoped threads can borrow self (and the context) since they are all joined before run returns
        thread::scope(|scope| loop {
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

            scope.spawn(move || self.serve_connection(conn, IpAddress::from(ip.octets())));
        })
    }
}

impl<C> Drop for TcpListener<'_, C> {
    fn drop(&mut self) {
        if self.socket >= 0 {
            unsafe { libc::close(self.socket) };
        }
    }
}
