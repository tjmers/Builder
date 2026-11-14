use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Networking::WinSock::{
    bind, closesocket, ioctlsocket, listen, setsockopt, shutdown, socket, AcceptEx, GetAcceptExSockaddrs, WSAEnumProtocolsW,
    WSAGetLastError, WSARecv, WSASend, WSASocketW, WSAStartup, AF_INET, FIONREAD, INADDR_ANY, INVALID_SOCKET, IPPROTO_TCP,
    SD_SEND, SOCKADDR, SOCKADDR_IN, SOCKET_ERROR, SOCK_STREAM, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT, WSABUF, WSADATA,
    WSAENOBUFS, WSAPROTOCOL_INFOW, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSA_IO_PENDING, WSA_OPERATION_ABORTED,
    XP1_IFS_HANDLES,
};
use windows_sys::Win32::Storage::FileSystem::SetFileCompletionNotificationModes;
use windows_sys::Win32::System::IO::{CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED};

use super::{
    log_closed_early, reserve_recv_room, AbortOnPanic, Framer, Framing, NativeSocket, RawRequestHandler, TcpListener, IDLE_TIMEOUT,
    RECV_BYTES, SEND_IDLE_TIMEOUT,
};
use crate::ip_address::IpAddress;

// Winsock 2.2
const WINSOCK_VERSION: u16 = 0x0202;

// The winsock2.h SOMAXCONN, which tells listen to use the largest backlog the system allows
// (windows-sys exposes the old winsock.h value of 5)
const SOMAXCONN: i32 = 0x7fff_ffff;

// How many AcceptEx calls are kept outstanding, so a burst of connections finds a socket already waiting for each one
// Anything past this waits in the listen backlog until a worker takes its next completion
const ACCEPT_POOL: usize = 64;

// The room AcceptEx needs for each of the local and remote addresses, which must be 16 bytes more than the address
const ADDRESS_BYTES: u32 = mem::size_of::<SOCKADDR_IN>() as u32 + 16;

// How much of a response each send hands to the kernel at most
// A send only completes once the client has taken all of it, so one send of a whole large response would show no
// progress until the very end, and a client downloading it slowly would look just like one that had stopped reading
// A piece is the least progress that can be seen, so a client has to take one every SEND_IDLE_TIMEOUT, which at this
// size is about 550 bytes a second, and anything slower is closed as having stopped reading
const SEND_BYTES: usize = 16 * 1024;

// How often connections are checked for ones idle past IDLE_TIMEOUT, and the accept pool topped back up if it fell short
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

// From winbase.h: a call that finishes immediately is handled by its caller, with no completion queued for it
const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 1;

// From winbase.h: how long GetQueuedCompletionStatus waits to mean forever
const INFINITE: u32 = u32::MAX;

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

fn close(conn: NativeSocket) {
    unsafe { closesocket(conn) };
}

// Whether the client has sent anything that has not been received yet
// If the socket cannot say, it counts as nothing, and the connection is closed the way it always was
fn has_unread(conn: NativeSocket) -> bool {
    let mut bytes = 0u32;
    unsafe { ioctlsocket(conn, FIONREAD, &mut bytes) == 0 && bytes > 0 }
}

// Tells the client the response ends here, while leaving the connection open to receive on
// A socket from AcceptEx has to be given its listening socket's properties before shutdown works on it, which is
// done here and not on every accept, since few connections need it
fn shutdown_sending(conn: NativeSocket, listen_socket: NativeSocket) -> io::Result<()> {
    let updated = unsafe {
        setsockopt(
            conn,
            SOL_SOCKET,
            SO_UPDATE_ACCEPT_CONTEXT,
            &listen_socket as *const NativeSocket as *const u8,
            mem::size_of::<NativeSocket>() as i32,
        )
    };
    if updated == SOCKET_ERROR || unsafe { shutdown(conn, SD_SEND) } == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(())
}

// Whether calls that finish immediately can safely skip their completion
// A layered socket provider (LSP) without real file handles can queue one anyway, which would hand a connection to
// two threads at once, so if any is installed every completion goes through the port instead
fn can_skip_completions() -> bool {
    let protocols = [IPPROTO_TCP, 0];
    let mut bytes = 0;
    let status = unsafe { WSAEnumProtocolsW(protocols.as_ptr(), ptr::null_mut(), &mut bytes) };
    if status != SOCKET_ERROR || unsafe { WSAGetLastError() } != WSAENOBUFS {
        return false;
    }
    let mut infos: Vec<WSAPROTOCOL_INFOW> = Vec::with_capacity(bytes as usize / mem::size_of::<WSAPROTOCOL_INFOW>() + 1);
    let count = unsafe { WSAEnumProtocolsW(protocols.as_ptr(), infos.as_mut_ptr(), &mut bytes) };
    if count == SOCKET_ERROR {
        return false;
    }
    // WSAEnumProtocolsW initialized the first count entries
    unsafe { infos.set_len(count as usize) };
    infos.iter().all(|info| info.dwServiceFlags1 & XP1_IFS_HANDLES != 0)
}

// The client's address out of the buffer AcceptEx filled in
fn peer_address(addresses: &[u8]) -> Option<Ipv4Addr> {
    let (mut local, mut remote) = (ptr::null_mut(), ptr::null_mut());
    let (mut local_len, mut remote_len) = (0, 0);
    unsafe {
        GetAcceptExSockaddrs(
            addresses.as_ptr().cast(),
            0,
            ADDRESS_BYTES,
            ADDRESS_BYTES,
            &mut local,
            &mut local_len,
            &mut remote,
            &mut remote_len,
        );
        if remote.is_null() || (remote_len as usize) < mem::size_of::<SOCKADDR_IN>() {
            return None;
        }
        // The address sits at an arbitrary offset in the buffer
        let client = ptr::read_unaligned(remote as *const SOCKADDR_IN);
        Some(Ipv4Addr::from(u32::from_be(client.sin_addr.S_un.S_addr)))
    }
}

// The overlapped operation a connection is waiting on
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Accept,
    Recv,
    Send,
    // Receiving what the client sends after its request, only to discard it (see Worker::linger)
    Drain,
}

// One connection, from the AcceptEx waiting for it until it is closed
// While an operation is outstanding the kernel owns it, through the pointer to its overlapped, which is why it is
// boxed and comes back through the completion
// repr(C) keeps overlapped first, so that pointer is also a pointer to the connection
#[repr(C)]
struct Connection<'a> {
    overlapped: OVERLAPPED,
    socket: NativeSocket,
    operation: Operation,
    // Where AcceptEx writes the addresses
    addresses: [u8; 2 * ADDRESS_BYTES as usize],
    ip: IpAddress,
    // The request received so far, and how far along it is
    request: Vec<u8>,
    framer: Framer,
    // Once the request is handled, the response and how much of it has been sent
    response: Option<(Cow<'a, [u8]>, usize)>,
    // When the whole response had been sent and the connection started waiting for the client to close (see
    // Worker::linger)
    closing_since: Option<Instant>,
}

impl Connection<'_> {
    fn new() -> Box<Self> {
        Box::new(Connection {
            overlapped: unsafe { mem::zeroed() },
            socket: INVALID_SOCKET,
            operation: Operation::Accept,
            addresses: [0; 2 * ADDRESS_BYTES as usize],
            ip: IpAddress::from([0; 4]),
            request: Vec::new(),
            framer: Framer::default(),
            response: None,
            closing_since: None,
        })
    }
}

// Starts receiving straight into the request's spare room, so there is no copy out of a separate chunk buffer
// and no zeroing of memory the kernel is about to overwrite
// operation is Recv for the request, or Drain for what follows it, which is discarded: the request buffer is empty
// by then and stays empty, since its length is never moved on over what arrives
// Returns the WSARecv status, after which conn must not be touched unless the call finished immediately
unsafe fn start_recv(conn: *mut Connection<'_>, operation: Operation, transferred: &mut u32) -> i32 {
    let conn = &mut *conn;
    conn.operation = operation;
    conn.overlapped = mem::zeroed();
    reserve_recv_room(&mut conn.request);
    let spare = conn.request.spare_capacity_mut();
    let buf = WSABUF { len: spare.len().min(u32::MAX as usize) as u32, buf: spare.as_mut_ptr().cast() };
    let mut flags = 0;
    WSARecv(conn.socket, &buf, 1, transferred, &mut flags, &mut conn.overlapped, None)
}

// Starts sending the next piece of what is left of the response, of at most SEND_BYTES
// The only copy is the one send makes into the kernel's socket buffer
// Returns the WSASend status, after which conn must not be touched unless the call finished immediately
unsafe fn start_send(conn: *mut Connection<'_>, transferred: &mut u32) -> i32 {
    let conn = &mut *conn;
    conn.operation = Operation::Send;
    conn.overlapped = mem::zeroed();
    let unsent = match &conn.response {
        Some((response, sent)) => &response[*sent..],
        None => &[],
    };
    let buf = WSABUF { len: unsent.len().min(SEND_BYTES) as u32, buf: unsent.as_ptr() as *mut u8 };
    WSASend(conn.socket, &buf, 1, transferred, 0, &mut conn.overlapped, None)
}

// What to do with a connection after its last operation finished
enum Next {
    Recv,
    Send,
    Drain,
    Close,
}

// A completion port handle, which the kernel lets every thread use at once
struct CompletionPort(HANDLE);
unsafe impl Send for CompletionPort {}
unsafe impl Sync for CompletionPort {}

// What the workers share
struct Shared<'l, 'a, C> {
    listener: &'l TcpListener<'a, C>,
    port: CompletionPort,
    skip_completions: bool,
    // How many AcceptEx calls are outstanding
    accepts: AtomicUsize,
    // Every accepted socket, when it last made progress, and whether it has a response to send, for the idle timeout
    // A socket is removed before it is closed, so holding the lock keeps every socket in here open
    last_active: Mutex<HashMap<NativeSocket, (Instant, bool)>>,
}

impl<C> Shared<'_, '_, C> {
    fn last_active(&self) -> MutexGuard<'_, HashMap<NativeSocket, (Instant, bool)>> {
        // Nothing panics while holding the lock, and the map is valid even if something did
        self.last_active.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Notes that socket just made progress, and whether it now has a response to send
    fn touch(&self, socket: NativeSocket, sending: bool) {
        self.last_active().insert(socket, (Instant::now(), sending));
    }

    // Cancels the outstanding operation of connections that have gone too long without making progress, and the
    // worker that gets the cancelled completion closes them
    // A client is allowed longer to take a response than to send a request, as on linux
    // Browsers open spare "preconnect" sockets they may never send a request on, so these are expected
    // A connection in its handler has nothing outstanding to cancel, and it is checked again on the next sweep
    fn close_idle(&self) {
        for (&socket, &(last_active, sending)) in self.last_active().iter() {
            let timeout = if sending { SEND_IDLE_TIMEOUT } else { IDLE_TIMEOUT };
            if last_active.elapsed() >= timeout {
                unsafe { CancelIoEx(socket as HANDLE, ptr::null()) };
            }
        }
    }
}

// One thread's view of the shared state, with a connection kept back to save allocating the next one
struct Worker<'s, 'l, 'a, C> {
    shared: &'s Shared<'l, 'a, C>,
    spare: Option<Box<Connection<'a>>>,
}

impl<'a, C> Worker<'_, '_, 'a, C> {
    // A new socket, ready to accept a connection and tied to the completion port
    fn open_socket(&self) -> io::Result<NativeSocket> {
        let flags = WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT;
        let conn = unsafe { WSASocketW(AF_INET as i32, SOCK_STREAM, IPPROTO_TCP, ptr::null(), 0, flags) };
        if conn == INVALID_SOCKET {
            return Err(last_wsa_error());
        }
        if unsafe { CreateIoCompletionPort(conn as HANDLE, self.shared.port.0, 0, 0) }.is_null() {
            let e = io::Error::last_os_error();
            close(conn);
            return Err(e);
        }
        if self.shared.skip_completions && unsafe { SetFileCompletionNotificationModes(conn as HANDLE, FILE_SKIP_COMPLETION_PORT_ON_SUCCESS) } == 0 {
            let e = io::Error::last_os_error();
            close(conn);
            return Err(e);
        }
        Ok(conn)
    }

    // Adds one AcceptEx to the pool, returning false if that failed
    // It always finishes through a completion, even when a connection is already waiting, since the skipping is only
    // turned on for connections and the listening socket does not have it
    // No SO_UPDATE_ACCEPT_CONTEXT call follows it, as that only matters to calls like shutdown, and the few
    // connections that need one make it then (see shutdown_sending)
    fn post_accept(&mut self) -> bool {
        let socket = match self.open_socket() {
            Ok(socket) => socket,
            Err(e) => {
                log_error!("Failed to open a socket to accept with: {}", e);
                return false;
            }
        };
        let mut conn = self.spare.take().unwrap_or_else(Connection::new);
        conn.socket = socket;
        conn.operation = Operation::Accept;
        conn.overlapped = unsafe { mem::zeroed() };

        self.shared.accepts.fetch_add(1, Ordering::Relaxed);
        let conn = Box::into_raw(conn);
        let mut received = 0;
        let status = unsafe {
            AcceptEx(
                self.shared.listener.socket,
                socket,
                (*conn).addresses.as_mut_ptr().cast(),
                0,
                ADDRESS_BYTES,
                ADDRESS_BYTES,
                &mut received,
                &mut (*conn).overlapped,
            )
        };
        if status == 0 && unsafe { WSAGetLastError() } != WSA_IO_PENDING {
            log_os_error("AcceptEx");
            self.shared.accepts.fetch_sub(1, Ordering::Relaxed);
            close(socket);
            self.spare = Some(unsafe { Box::from_raw(conn) });
            return false;
        }
        true
    }

    // Takes completions until the port itself fails
    fn run(&mut self) {
        loop {
            let (mut transferred, mut key, mut overlapped) = (0, 0, ptr::null_mut());
            let ok = unsafe { GetQueuedCompletionStatus(self.shared.port.0, &mut transferred, &mut key, &mut overlapped, INFINITE) };
            if overlapped.is_null() {
                log_error!("GetQueuedCompletionStatus: {}", io::Error::last_os_error());
                return;
            }
            let result = if ok != 0 { Ok(transferred) } else { Err(io::Error::last_os_error()) };
            // The kernel is done with this connection, so it belongs to this thread again
            let conn = unsafe { Box::from_raw(overlapped as *mut Connection<'a>) };
            self.advance(conn, result);
        }
    }

    // Moves conn along until it has to wait on the kernel or is finished
    // result is how its last operation went: the bytes it transferred, or why it failed
    fn advance(&mut self, mut conn: Box<Connection<'a>>, mut result: io::Result<u32>) {
        loop {
            let next = match conn.operation {
                Operation::Accept => self.accepted(&mut conn, result),
                Operation::Recv => self.received(&mut conn, result),
                Operation::Send => self.sent(&mut conn, result),
                Operation::Drain => self.drained(&mut conn, result),
            };

            // Ownership passes to the kernel before the call, since another worker can have the completion before
            // the call has even returned here
            let raw = Box::into_raw(conn);
            let mut transferred = 0;
            let status = match next {
                Next::Recv => unsafe { start_recv(raw, Operation::Recv, &mut transferred) },
                Next::Drain => unsafe { start_recv(raw, Operation::Drain, &mut transferred) },
                Next::Send => unsafe { start_send(raw, &mut transferred) },
                Next::Close => {
                    self.finish(unsafe { Box::from_raw(raw) });
                    return;
                }
            };
            if status == SOCKET_ERROR {
                let e = unsafe { WSAGetLastError() };
                if e == WSA_IO_PENDING {
                    return;
                }
                result = Err(io::Error::from_raw_os_error(e));
            } else if self.shared.skip_completions {
                result = Ok(transferred);
            } else {
                // It finished immediately, but a completion is on its way anyway
                return;
            }
            // The call finished immediately, so no completion is coming and the connection is still this thread's
            conn = unsafe { Box::from_raw(raw) };
        }
    }

    // An AcceptEx finished, which takes one out of the pool, so one is added back before serving this connection
    // If that fails the pool stays short until the next sweep tops it up
    fn accepted(&mut self, conn: &mut Connection<'a>, result: io::Result<u32>) -> Next {
        self.shared.accepts.fetch_sub(1, Ordering::Relaxed);
        self.post_accept();

        if let Err(e) = result {
            // If there is an error here do not shut down the server just continue
            log_error!("accept: {}", e);
            return Next::Close;
        }
        let ip = peer_address(&conn.addresses).unwrap_or(Ipv4Addr::UNSPECIFIED);
        log_info!("Connection from {}", ip);
        conn.ip = IpAddress::from(ip.octets());
        self.shared.touch(conn.socket, false);
        Next::Recv
    }

    fn received(&mut self, conn: &mut Connection<'a>, result: io::Result<u32>) -> Next {
        let n = match result {
            Ok(n) => n as usize,
            // Cancelled by the idle sweep
            Err(e) if e.raw_os_error() == Some(WSA_OPERATION_ABORTED) => {
                log_info!("Closing connection idle for {:?} after receiving {} bytes", IDLE_TIMEOUT, conn.request.len());
                return Next::Close;
            }
            Err(e) => {
                log_error!("recv: {}", e);
                return Next::Close;
            }
        };
        let len = conn.request.len();
        if n == 0 {
            log_closed_early(len);
            return Next::Close;
        }
        // The kernel initialized the first n bytes of the spare capacity
        unsafe { conn.request.set_len(len + n) };

        let total = match conn.framer.advance(&conn.request) {
            Framing::Complete(total) => total,
            Framing::TooLarge => return Next::Close,
            Framing::Incomplete => {
                self.shared.touch(conn.socket, false);
                return Next::Recv;
            }
        };
        let Some(response) = self.shared.listener.handle(&mut conn.request, total, conn.ip) else {
            return Next::Close;
        };
        // Sending nothing would complete with no bytes sent, which is otherwise a failed send
        if response.is_empty() {
            return Next::Close;
        }
        conn.response = Some((response, 0));
        // The time spent in the handler is not the client being idle
        self.shared.touch(conn.socket, true);
        Next::Send
    }

    fn sent(&mut self, conn: &mut Connection<'a>, result: io::Result<u32>) -> Next {
        let Some((response, sent)) = &mut conn.response else {
            return Next::Close;
        };
        match result {
            Ok(0) => {
                log_error!("send: no bytes were sent");
                Next::Close
            }
            Ok(n) => {
                *sent += n as usize;
                if *sent < response.len() {
                    self.shared.touch(conn.socket, true);
                    return Next::Send;
                }
                self.linger(conn)
            }
            // Cancelled by the idle sweep
            Err(e) if e.raw_os_error() == Some(WSA_OPERATION_ABORTED) => {
                log_info!("Closing connection that stopped reading after {} of {} bytes", sent, response.len());
                Next::Close
            }
            Err(e) => {
                log_error!("send: {}", e);
                Next::Close
            }
        }
    }

    // Decides what to do once the whole response has been handed to the kernel
    // A send completes when the response is in the socket's buffer, not when the client has it, and closing a
    // socket while the client has sent something that was never read (a pipelined request, a line ending after a
    // body) resets the connection and throws away whatever is still in that buffer
    // So if anything is unread, the client is told the response ends here and the connection stays open, discarding
    // what the client sends, until the client closes its end or SEND_IDLE_TIMEOUT passes
    // Otherwise it is closed straight away, which is nearly every connection
    fn linger(&mut self, conn: &mut Connection<'a>) -> Next {
        if !has_unread(conn.socket) {
            return Next::Close;
        }
        if let Err(e) = shutdown_sending(conn.socket, self.shared.listener.socket) {
            log_error!("shutdown: {}", e);
            return Next::Close;
        }
        // The kernel has its own copy of what is left to send
        conn.response = None;
        conn.closing_since = Some(Instant::now());
        // The sweep allows it as long from here as it allows for a piece of the response
        self.shared.touch(conn.socket, true);
        Next::Drain
    }

    // Some of what the client sent after its request was received, and is discarded
    fn drained(&mut self, conn: &mut Connection<'a>, result: io::Result<u32>) -> Next {
        match result {
            // The client has closed its end, so nothing more can arrive to go unread, and closing from here on
            // sends the rest of the response as usual
            Ok(0) => Next::Close,
            // Nothing the client sends counts as progress, so one that sends without stopping is closed here, where
            // the sweep would rarely find a receive outstanding to cancel
            Ok(_) if conn.closing_since.is_some_and(|since| since.elapsed() < SEND_IDLE_TIMEOUT) => Next::Drain,
            Ok(_) => {
                log_info!("Closing connection that had not closed {:?} after the end of its response", SEND_IDLE_TIMEOUT);
                Next::Close
            }
            // Cancelled by the idle sweep
            Err(e) if e.raw_os_error() == Some(WSA_OPERATION_ABORTED) => {
                log_info!("Closing connection that had not closed {:?} after the end of its response", SEND_IDLE_TIMEOUT);
                Next::Close
            }
            // The client reset the connection, or something else that leaves nothing more to wait for
            Err(_) => Next::Close,
        }
    }

    // Closes a finished connection, keeping it to reuse for the next accept if there isn't one kept already
    fn finish(&mut self, mut conn: Box<Connection<'a>>) {
        self.shared.last_active().remove(&conn.socket);
        close(conn.socket);
        if self.spare.is_none() && conn.request.capacity() <= RECV_BYTES {
            conn.request.clear();
            conn.framer = Framer::default();
            conn.response = None;
            conn.closing_since = None;
            self.spare = Some(conn);
        }
    }
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
}

// C must be Sync because the workers all share the context
impl<C: Sync> TcpListener<'_, C> {
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

        // A completion port runs at most one worker per core at a time (0 asks for that default)
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 0) };
        if port.is_null() {
            log_error!("CreateIoCompletionPort: {}", io::Error::last_os_error());
            return false;
        }
        if unsafe { CreateIoCompletionPort(self.socket as HANDLE, port, 0, 0) }.is_null() {
            log_error!("CreateIoCompletionPort: {}", io::Error::last_os_error());
            unsafe { CloseHandle(port) };
            return false;
        }
        log_info!("Now serving on port {}", self.port);

        let shared = Shared {
            listener: self,
            port: CompletionPort(port),
            skip_completions: can_skip_completions(),
            accepts: AtomicUsize::new(0),
            last_active: Mutex::new(HashMap::new()),
        };
        // Twice the workers the port runs at once: when a handler blocks on something the port lets another worker
        // take over its core, so handlers have to block across all of them to hold up other connections
        // Spawning a thread for every connection cost more than serving the request, and a connection that is slow
        // or never sends anything (like a browser's preconnect socket) only takes up a map entry, not a thread
        let workers = 2 * thread::available_parallelism().map_or(4, |n| n.get());

        // Scoped threads can borrow self (and the context) since they are all joined before run returns
        // A panic in any of them, or in this one, ends the process: the scope would otherwise wait forever for the
        // threads that are left, with the port still open
        let _abort = AbortOnPanic;
        thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    let _abort = AbortOnPanic;
                    Worker { shared: &shared, spare: None }.run()
                });
            }

            // The calling thread keeps the pool full, including filling it to start with, and sweeps for idle connections
            let mut sweeper = Worker { shared: &shared, spare: None };
            loop {
                while shared.accepts.load(Ordering::Relaxed) < ACCEPT_POOL {
                    if !sweeper.post_accept() {
                        break;
                    }
                }
                thread::sleep(SWEEP_INTERVAL);
                shared.close_idle();
            }
        })
    }
}

impl<C> Drop for TcpListener<'_, C> {
    fn drop(&mut self) {
        if self.socket != INVALID_SOCKET {
            unsafe { closesocket(self.socket) };
        }
    }
}
