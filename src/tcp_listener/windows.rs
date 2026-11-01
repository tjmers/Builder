use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::sync::OnceLock;

use windows_sys::Win32::Networking::WinSock::{
    accept, bind, closesocket, listen, recv as wsa_recv, send as wsa_send, socket, WSAGetLastError, WSAStartup,
    AF_INET, INADDR_ANY, INVALID_SOCKET, SOCKADDR, SOCKADDR_IN, SOCKET_ERROR, SOCK_STREAM, WSADATA,
};

use super::{NativeSocket, RawRequestHandler, TcpListener};
use crate::ip_address::IpAddress;

// Winsock 2.2
const WINSOCK_VERSION: u16 = 0x0202;

// The winsock2.h SOMAXCONN, which tells listen to use the largest backlog the system allows
// (windows-sys exposes the old winsock.h value of 5)
const SOMAXCONN: i32 = 0x7fff_ffff;

// The last Winsock error as an io::Error
fn last_wsa_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

// Logs the last Winsock error, prefixed by the call that failed
fn log_os_error(call: &str) {
    log_error!("{}: {}", call, last_wsa_error());
}

// Winsock must be started before any socket call
// It is started once for the whole process and left running until the process exits
fn startup() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let mut data: WSADATA = unsafe { mem::zeroed() };
        // WSAStartup returns its error directly instead of setting WSAGetLastError
        let status = unsafe { WSAStartup(WINSOCK_VERSION, &mut data) };
        if status != 0 {
            log_error!("WSAStartup: {}", io::Error::from_raw_os_error(status));
        }
        status == 0
    })
}

// Receives into buf, returning the number of bytes read (0 once the peer has closed)
pub(super) fn recv(conn: NativeSocket, buf: &mut [u8]) -> io::Result<usize> {
    let len = buf.len().min(i32::MAX as usize) as i32;
    let n = unsafe { wsa_recv(conn, buf.as_mut_ptr(), len, 0) };
    if n == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(n as usize)
}

// Sends as much of data as the OS accepts, returning the number of bytes sent
// Windows has no SIGPIPE, so unlike linux no flag is needed for a client that already hung up
pub(super) fn send(conn: NativeSocket, data: &[u8]) -> io::Result<usize> {
    let len = data.len().min(i32::MAX as usize) as i32;
    let n = unsafe { wsa_send(conn, data.as_ptr(), len, 0) };
    if n == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(n as usize)
}

impl<'a, C> TcpListener<'a, C> {
    pub fn new(context: &'a C, handler: RawRequestHandler<'a, C>) -> Self {
        // Create the socket but do not start listening
        let socket = if startup() {
            let socket = unsafe { socket(AF_INET as i32, SOCK_STREAM, 0) };
            if socket == INVALID_SOCKET {
                log_os_error("socket");
            }
            socket
        } else {
            INVALID_SOCKET
        };

        TcpListener {
            socket,
            handler,
            context,
            port: -1,
        }
    }

    pub fn bind(&mut self, port: i32) -> bool {
        if self.socket == INVALID_SOCKET {
            return false;
        }
        // SO_REUSEADDR is deliberately not set: on Windows it lets a socket bind to a port another socket
        // is actively using, rather than only allowing a quick rebind past TIME_WAIT like on linux
        // Windows already allows that rebind by default

        let mut addr: SOCKADDR_IN = unsafe { mem::zeroed() };
        addr.sin_family = AF_INET;
        addr.sin_addr.S_un.S_addr = INADDR_ANY.to_be();
        addr.sin_port = (port as u16).to_be();

        // Bind
        let bind_status = unsafe {
            bind(
                self.socket,
                &addr as *const SOCKADDR_IN as *const SOCKADDR,
                mem::size_of::<SOCKADDR_IN>() as i32,
            )
        };
        if bind_status == SOCKET_ERROR {
            log_os_error("bind");
            unsafe { closesocket(self.socket) };
            self.socket = INVALID_SOCKET;
            return false;
        }
        self.port = port;
        true
    }

    pub fn valid(&self) -> bool {
        self.socket != INVALID_SOCKET
    }

    pub fn run(&self) -> bool {
        // Start listening
        if self.socket == INVALID_SOCKET || self.port < 0 {
            return false;
        }
        let listen_status = unsafe { listen(self.socket, SOMAXCONN) };
        if listen_status == SOCKET_ERROR {
            log_os_error("listen");
            return false;
        }
        log_info!("Now serving on port {}", self.port);

        loop {
            let mut client: SOCKADDR_IN = unsafe { mem::zeroed() };
            let mut client_len = mem::size_of::<SOCKADDR_IN>() as i32;
            let conn = unsafe {
                accept(
                    self.socket,
                    &mut client as *mut SOCKADDR_IN as *mut SOCKADDR,
                    &mut client_len,
                )
            };
            if conn == INVALID_SOCKET {
                log_os_error("accept");
                // If there is an error here do not shut down the server just continue
                continue;
            }

            let ip = Ipv4Addr::from(u32::from_be(unsafe { client.sin_addr.S_un.S_addr }));

            log_info!("Connection from {}:{}", ip, u16::from_be(client.sin_port));

            self.serve(conn, IpAddress::from(ip.octets()));

            unsafe { closesocket(conn) };
        }
    }
}

impl<C> Drop for TcpListener<'_, C> {
    fn drop(&mut self) {
        if self.socket != INVALID_SOCKET {
            unsafe { closesocket(self.socket) };
        }
    }
}
