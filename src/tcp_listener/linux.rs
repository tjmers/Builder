use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::mem::{self, MaybeUninit};
use std::net::Ipv4Addr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use libc::{c_int, c_void, epoll_event, sockaddr, sockaddr_in, socklen_t};

use super::{log_closed_early, request_len, Framing, NativeSocket, RawRequestHandler, TcpListener, IDLE_TIMEOUT};
use crate::ip_address::IpAddress;

const INVALID_SOCKET: NativeSocket = -1;

// The epoll token for the listening socket; connections use their fd, which is never negative
const LISTENER_TOKEN: u64 = u64::MAX;

// How many ready sockets one epoll_wait call can return
const MAX_EVENTS: usize = 64;

// How much each recv reads at most, enough for a typical request with a small body in one call
const RECV_BYTES: usize = 16 * 1024;

// How often a worker with open connections checks them for ones idle past IDLE_TIMEOUT
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

// How long the kernel holds a new connection back from accept while waiting for its first data (TCP_DEFER_ACCEPT)
// After this (one SYN-ACK retransmit) it is handed over anyway, so a connection that never sends anything still
// reaches a worker and is closed after IDLE_TIMEOUT
const DEFER_ACCEPT_SECS: c_int = 1;

// Responses up to this size are closed without TIME_WAIT (see bind_shared), larger ones get a normal close
// The fast close resets the connection once the client has acknowledged the whole response, and a client that
// discards unread data on a reset (Windows does) could lose the end of a large response it is still reading
const FAST_CLOSE_MAX_BYTES: usize = 64 * 1024;

// How often the rescue thread checks for workers that have stopped making progress (see Worker::rescue)
const RESCUE_INTERVAL: Duration = Duration::from_millis(10);

// The longest a blocking accept on a listening socket waits (SO_RCVTIMEO) before giving up with EAGAIN
// Bounds how long the rescue thread can be stuck if a worker takes the connection it was about to accept
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(1);

// Logs the last OS error, prefixed by the call that failed
fn log_os_error(call: &str) {
    log_error!("{}: {}", call, io::Error::last_os_error());
}

// Receives into buf, returning the number of bytes read (0 once the peer has closed)
// buf may be uninitialized: the kernel only writes to it, and the first n bytes are initialized on return
fn recv(conn: NativeSocket, buf: &mut [MaybeUninit<u8>]) -> io::Result<usize> {
    let n = unsafe { libc::recv(conn, buf.as_mut_ptr() as *mut c_void, buf.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

// Sends as much of data as the OS accepts, returning the number of bytes sent
fn send(conn: NativeSocket, data: &[u8]) -> io::Result<usize> {
    // MSG_NOSIGNAL so a client that already hung up gives an error instead of a SIGPIPE that kills the server
    // MSG_MORE holds back the final partial segment, so the FIN sent when the connection is closed straight after
    // rides on it instead of going out as a packet of its own
    let flags = libc::MSG_NOSIGNAL | libc::MSG_MORE;
    let n = unsafe { libc::send(conn, data.as_ptr() as *const c_void, data.len(), flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn close(conn: NativeSocket) {
    unsafe { libc::close(conn) };
}

// Sets an int socket option
fn set_option(socket: NativeSocket, level: c_int, option: c_int, value: c_int) -> io::Result<()> {
    let status = unsafe {
        libc::setsockopt(
            socket,
            level,
            option,
            &value as *const c_int as *const c_void,
            mem::size_of::<c_int>() as socklen_t,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Binds socket to port on every interface, with SO_REUSEPORT so each worker can bind its own listening socket
// to the same port
// The TCP options set here are inherited by every connection accepted from the socket
fn bind_shared(socket: NativeSocket, port: i32) -> io::Result<()> {
    set_option(socket, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
    set_option(socket, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1)?;
    // Only queue a connection for accept once its first data has arrived, so the worker is woken once with the
    // request ready to read, instead of once for the handshake and again (after an EAGAIN and an epoll_ctl) for the request
    set_option(socket, libc::IPPROTO_TCP, libc::TCP_DEFER_ACCEPT, DEFER_ACCEPT_SECS)?;
    // The server closes first after every response, which normally leaves the connection in TIME_WAIT for a minute
    // A negative TCP_LINGER2 instead forgets the connection as soon as the client has acknowledged the whole response
    // and the FIN, skipping TIME_WAIT
    // Without it every port a client reuses lands on one of these TIME_WAIT sockets, and now and then the kernel drops
    // the new SYN, stalling that client's connect for the full one second SYN retransmit timeout
    // It also saves creating and tearing down the TIME_WAIT socket and the client's FIN, one packet per connection
    // The trade-off: the client's ACK of the FIN is answered with a reset, and a client that discards unread data on
    // a reset could lose a response it has received but not yet read (Linux keeps it), so large responses opt out
    // in Connection::set_response
    set_option(socket, libc::IPPROTO_TCP, libc::TCP_LINGER2, -1)?;

    let mut addr: sockaddr_in = unsafe { mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
    addr.sin_port = (port as u16).to_be();
    let status = unsafe {
        libc::bind(
            socket,
            &addr as *const sockaddr_in as *const sockaddr,
            mem::size_of::<sockaddr_in>() as socklen_t,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Fails if another socket is already listening on port, by binding a plain socket there
// SO_REUSEADDR lets it past connections lingering in TIME_WAIT, but not past a listener, with or without SO_REUSEPORT
fn check_port_free(port: i32) -> io::Result<()> {
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = set_option(socket, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1).and_then(|()| {
        let mut addr: sockaddr_in = unsafe { mem::zeroed() };
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
        addr.sin_port = (port as u16).to_be();
        let status = unsafe {
            libc::bind(socket, &addr as *const sockaddr_in as *const sockaddr, mem::size_of::<sockaddr_in>() as socklen_t)
        };
        if status < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    });
    close(socket);
    result
}

// Starts listening on a bound socket
fn start_listening(socket: NativeSocket) -> io::Result<()> {
    if unsafe { libc::listen(socket, libc::SOMAXCONN) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // A new connection starts out acknowledging data straight away, so the kernel would answer the request with an
    // ACK of its own before the worker had even accepted it, and the response (which carries the same ACK) follows
    // moments later
    // Turning quick ACKs off on the listener lets the connections it accepts inherit that, which saves a packet per
    // connection; the ACK still goes out within the delayed ACK timeout if the handler takes that long
    // It must come after listen, which resets the setting
    set_option(socket, libc::IPPROTO_TCP, libc::TCP_QUICKACK, 0)?;
    // A worker with nothing else to do waits in accept itself, and this bounds that wait (see ACCEPT_TIMEOUT)
    let timeout = libc::timeval { tv_sec: ACCEPT_TIMEOUT.as_secs() as libc::time_t, tv_usec: 0 };
    let status = unsafe {
        libc::setsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &timeout as *const libc::timeval as *const c_void,
            mem::size_of::<libc::timeval>() as socklen_t,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Switches socket between blocking and non-blocking
fn set_nonblocking(socket: NativeSocket, nonblocking: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(socket, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if nonblocking { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
    if unsafe { libc::fcntl(socket, libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// A listening socket bound to port, sharing it with the others through SO_REUSEPORT
// It starts out blocking, since its worker waits in accept while it has no other connections
fn open_listener(port: i32) -> io::Result<NativeSocket> {
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = bind_shared(socket, port).and_then(|()| start_listening(socket));
    if let Err(e) = result {
        close(socket);
        return Err(e);
    }
    Ok(socket)
}

// Adds fd to epoll, or changes what it is waiting for if it is already there
fn epoll_ctl(epoll: c_int, op: c_int, fd: c_int, events: u32, token: u64) -> io::Result<()> {
    let mut event = epoll_event { events, u64: token };
    if unsafe { libc::epoll_ctl(epoll, op, fd, &mut event) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// What an accepted connection is waiting on
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interest {
    // Not registered with epoll yet
    None,
    Read,
    Write,
}

impl Interest {
    fn events(self) -> u32 {
        match self {
            Interest::None => 0,
            Interest::Read => libc::EPOLLIN as u32,
            Interest::Write => libc::EPOLLOUT as u32,
        }
    }
}

// A connection owned by one worker, from accept until it is closed
struct Connection<'a> {
    ip: IpAddress,
    // The request received so far
    request: Vec<u8>,
    // Once the request is handled, the response and how much of it has been sent
    response: Option<(Cow<'a, [u8]>, usize)>,
    interest: Interest,
    // When the connection last made progress, for the idle timeout
    last_active: Instant,
}

// What to do with a connection after driving it as far as it can go without blocking
enum Next {
    // Keep it and wait for the socket to be ready for this
    Wait(Interest),
    // It is finished (or failed) and should be closed
    Close,
}

impl<'a> Connection<'a> {
    // request is an empty buffer to receive into, recycled from a finished connection when there is one
    fn new(ip: IpAddress, request: Vec<u8>) -> Self {
        Connection {
            ip,
            request,
            response: None,
            interest: Interest::None,
            last_active: Instant::now(),
        }
    }

    // Receives until the request is complete or the socket has nothing more for now
    // Returns the request's length once it is complete
    fn recv_request(&mut self, fd: NativeSocket) -> Result<Option<usize>, ()> {
        loop {
            match request_len(&self.request) {
                Framing::Complete(total) => return Ok(Some(total)),
                Framing::TooLarge => return Err(()),
                Framing::Incomplete => {}
            }
            // Receive straight into the request's spare room, so there is no copy out of a separate chunk buffer
            // and no zeroing of memory the kernel is about to overwrite
            let len = self.request.len();
            self.request.reserve(RECV_BYTES);
            let received = recv(fd, self.request.spare_capacity_mut());
            if let Ok(n) = received {
                // The kernel initialized the first n bytes of the spare capacity
                unsafe { self.request.set_len(len + n) };
            }
            match received {
                Ok(0) => {
                    log_closed_early(len);
                    return Err(());
                }
                Ok(_) => self.last_active = Instant::now(),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log_error!("recv: {}", e);
                    return Err(());
                }
            }
        }
    }

    // Stores the response to send
    // A large response gets a normal close with TIME_WAIT instead of the fast close set up in bind_shared
    fn set_response(&mut self, fd: NativeSocket, response: Cow<'a, [u8]>) {
        if response.len() > FAST_CLOSE_MAX_BYTES {
            // 0 means the system default
            if let Err(e) = set_option(fd, libc::IPPROTO_TCP, libc::TCP_LINGER2, 0) {
                log_error!("Failed to restore the default close for a large response: {}", e);
            }
        }
        self.response = Some((response, 0));
    }

    // Sends as much of the response as the socket takes
    // Returns true once all of it has been sent
    fn send_response(&mut self, fd: NativeSocket) -> Result<bool, ()> {
        let Some((response, sent)) = &mut self.response else {
            return Ok(true);
        };
        while *sent < response.len() {
            match send(fd, &response[*sent..]) {
                Ok(0) => {
                    log_error!("send: no bytes were sent");
                    return Err(());
                }
                Ok(n) => {
                    *sent += n;
                    self.last_active = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log_error!("send: {}", e);
                    return Err(());
                }
            }
        }
        Ok(true)
    }
}

impl<'a, C> TcpListener<'a, C> {
    pub fn new(context: &'a C, handler: RawRequestHandler<'a, C>) -> Self {
        // Create the socket but do not start listening
        // It starts out blocking, since its worker waits in accept while it has no other connections
        let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
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
        // SO_REUSEPORT would let this join another server already listening on the port and silently split its
        // connections, so first make sure nothing is
        if let Err(e) = check_port_free(port) {
            log_error!("bind: port {} is unavailable: {}", port, e);
            unsafe { libc::close(self.socket) };
            self.socket = INVALID_SOCKET;
            return false;
        }
        if let Err(e) = bind_shared(self.socket, port) {
            log_error!("bind: {}", e);
            unsafe { libc::close(self.socket) };
            self.socket = INVALID_SOCKET;
            return false;
        }
        self.port = port;
        true
    }

    pub fn valid(&self) -> bool {
        self.socket >= 0
    }

    // Accepts one pending connection from a listening socket, if there is one
    fn accept(&self, listen_socket: NativeSocket) -> Option<(NativeSocket, IpAddress)> {
        let mut client: sockaddr_in = unsafe { mem::zeroed() };
        let mut client_len = mem::size_of::<sockaddr_in>() as socklen_t;
        let conn = unsafe {
            libc::accept4(
                listen_socket,
                &mut client as *mut sockaddr_in as *mut sockaddr,
                &mut client_len,
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if conn < 0 {
            let e = io::Error::last_os_error();
            // WouldBlock is also a blocking accept reaching ACCEPT_TIMEOUT, and Interrupted a signal during one
            if !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
                // If there is an error here do not shut down the server just continue
                log_error!("accept: {}", e);
            }
            return None;
        }

        let ip = Ipv4Addr::from(u32::from_be(client.sin_addr.s_addr));
        log_info!("Connection from {}:{}", ip, u16::from_be(client.sin_port));
        Some((conn, IpAddress::from(ip.octets())))
    }

    // Moves a connection along as far as it can go without blocking: receive the request, run the handler, send
    fn drive(&self, fd: NativeSocket, conn: &mut Connection<'a>) -> Next {
        if conn.response.is_none() {
            let total = match conn.recv_request(fd) {
                Ok(Some(total)) => total,
                Ok(None) => return Next::Wait(Interest::Read),
                Err(()) => return Next::Close,
            };
            // Anything past the body (e.g. a pipelined request) is discarded since the connection is closed after responding
            conn.request.truncate(total);

            // A handler that panics only takes down its own connection, not the worker serving many others
            let handled = panic::catch_unwind(AssertUnwindSafe(|| (self.handler)(self.context, &conn.request, conn.ip)));
            let Ok(response) = handled else {
                log_error!("The request handler panicked");
                return Next::Close;
            };
            // The buffer is kept for the next connection, unless an unusually large request grew it
            if conn.request.capacity() > RECV_BYTES {
                conn.request = Vec::new();
            } else {
                conn.request.clear();
            }
            conn.set_response(fd, response);
        }

        match conn.send_response(fd) {
            Ok(true) => Next::Close,
            Ok(false) => Next::Wait(Interest::Write),
            Err(()) => Next::Close,
        }
    }
}

// SO_INCOMING_CPU from the generic socket ABI these architectures use (the libc crate does not export it)
#[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "arm", target_arch = "riscv64"))]
const SO_INCOMING_CPU: c_int = 49;

// The CPU each worker runs on: one worker per CPU this process may use, or unpinned workers if that is unknown
fn worker_cpus() -> Vec<Option<usize>> {
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &mut set) } == 0 {
        let cpus: Vec<_> = (0..libc::CPU_SETSIZE as usize).filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) }).map(Some).collect();
        if !cpus.is_empty() {
            return cpus;
        }
    } else {
        log_os_error("sched_getaffinity");
    }
    vec![None; thread::available_parallelism().map_or(4, |n| n.get())]
}

// Runs the calling thread only on cpu
fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    if unsafe { libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Runs the calling thread under SCHED_IDLE, so it only gets its CPU when no ordinary thread wants it
// A worker serves the connections from clients on its own CPU, and closing a connection wakes the client blocked
// reading the response
// While the worker holds the CPU the scheduler counts it as busy and moves the woken client to another, often idle,
// CPU, which costs a cross-CPU interrupt (expensive in a VM) and takes the client's next connection along with it
// A CPU running only SCHED_IDLE threads counts as free, so the client is woken right where it was and runs straight
// away, and the worker carries on once the client blocks again
// An unprivileged thread cannot switch back to a normal policy, which is why the rescue thread exists
fn deprioritize_worker() -> io::Result<()> {
    let param = libc::sched_param { sched_priority: 0 };
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Has the kernel hand this listening socket the connections whose packets are processed on cpu
// The worker pinned to that CPU then serves the connection on the same core as the network stack (and, for a local
// client, as the client itself), so waking each other needs no cross-CPU interrupt and the socket stays in that
// core's cache
// Connections on a CPU without a steered listener fall back to the usual SO_REUSEPORT hash
#[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "arm", target_arch = "riscv64"))]
fn steer_to_cpu(socket: NativeSocket, cpu: usize) -> io::Result<()> {
    set_option(socket, libc::SOL_SOCKET, SO_INCOMING_CPU, cpu as c_int)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64", target_arch = "arm", target_arch = "riscv64")))]
fn steer_to_cpu(_socket: NativeSocket, _cpu: usize) -> io::Result<()> {
    Ok(())
}

// Counts a worker's trips around its event loop, so the rescue thread can tell when it has stalled
// Aligned to a cache line so workers bumping their own counters never contend
#[repr(align(64))]
#[derive(Default)]
struct Progress(AtomicU64);

// A listening socket the rescue thread watches, with its worker's progress at the last check
struct Watched<'p> {
    socket: NativeSocket,
    progress: &'p Progress,
    seen: u64,
}

// One thread's event loop state: its epoll instance and the connections it has accepted
// Workers each accept from their own listening socket, while the rescue thread has none and only takes over
// connections waiting on a stalled worker's socket
struct Worker<'l, 'a, 'p, C> {
    listener: &'l TcpListener<'a, C>,
    // This worker's own listening socket, or INVALID_SOCKET for the rescue thread
    listen_socket: NativeSocket,
    // The first worker uses the listener's socket, which the listener closes
    owns_listen_socket: bool,
    epoll: c_int,
    connections: HashMap<NativeSocket, Connection<'a>>,
    // Bumped every trip around the event loop, None for the rescue thread
    progress: Option<&'p Progress>,
    // The workers' listening sockets, only for the rescue thread
    watched: Vec<Watched<'p>>,
    // Whether listen_socket is non-blocking and registered with epoll, which it only is while there are connections
    listener_in_epoll: bool,
    // A finished connection's receive buffer, reused for the next one so most connections do not allocate one
    spare: Vec<u8>,
}

impl<'l, 'a, 'p, C> Worker<'l, 'a, 'p, C> {
    fn new(listener: &'l TcpListener<'a, C>, listen_socket: NativeSocket, owns_listen_socket: bool) -> Option<Self> {
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            log_os_error("epoll_create1");
            return None;
        }
        Some(Worker {
            listener,
            listen_socket,
            owns_listen_socket,
            epoll,
            connections: HashMap::new(),
            progress: None,
            watched: Vec::new(),
            listener_in_epoll: false,
            spare: Vec::new(),
        })
    }

    fn is_rescue(&self) -> bool {
        self.listen_socket == INVALID_SOCKET
    }

    fn bump_progress(&self) {
        if let Some(progress) = self.progress {
            progress.0.store(progress.0.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        }
    }

    // Moves the listening socket between waiting in a blocking accept (no connections open) and waiting in epoll
    // alongside the open connections
    // A blocking accept is one syscall per connection instead of epoll_wait and then accept, and connections are
    // usually served and closed straight from accept, so the switch to epoll is rare
    fn set_listener_in_epoll(&mut self, in_epoll: bool) -> io::Result<()> {
        if in_epoll == self.listener_in_epoll {
            return Ok(());
        }
        if in_epoll {
            set_nonblocking(self.listen_socket, true)?;
            epoll_ctl(self.epoll, libc::EPOLL_CTL_ADD, self.listen_socket, libc::EPOLLIN as u32, LISTENER_TOKEN)?;
        } else {
            epoll_ctl(self.epoll, libc::EPOLL_CTL_DEL, self.listen_socket, 0, LISTENER_TOKEN)?;
            set_nonblocking(self.listen_socket, false)?;
        }
        self.listener_in_epoll = in_epoll;
        Ok(())
    }

    fn new_connection(&mut self, ip: IpAddress) -> Connection<'a> {
        Connection::new(ip, mem::take(&mut self.spare))
    }

    fn run(&mut self) {
        let mut events = [epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
        let mut last_sweep = Instant::now();
        let mut last_rescue = Instant::now();
        loop {
            if !self.is_rescue() {
                let in_epoll = !self.connections.is_empty();
                if let Err(e) = self.set_listener_in_epoll(in_epoll) {
                    // Stay in whichever mode the socket is still in
                    log_error!("Failed to switch the listener {} epoll: {}", if in_epoll { "into" } else { "out of" }, e);
                }
                if !self.listener_in_epoll {
                    // With no connections open there is nothing to time out, so just wait for the next one
                    let accepted = self.listener.accept(self.listen_socket);
                    self.bump_progress();
                    if let Some((fd, ip)) = accepted {
                        let conn = self.new_connection(ip);
                        self.advance(fd, conn);
                    }
                    continue;
                }
            }
            let timeout = if self.is_rescue() {
                RESCUE_INTERVAL.as_millis() as c_int
            } else {
                SWEEP_INTERVAL.as_millis() as c_int
            };
            let n = unsafe { libc::epoll_wait(self.epoll, events.as_mut_ptr(), MAX_EVENTS as c_int, timeout) };
            self.bump_progress();
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    log_error!("epoll_wait: {}", e);
                }
                continue;
            }

            for event in &events[..n as usize] {
                if event.u64 == LISTENER_TOKEN {
                    if let Some((fd, ip)) = self.listener.accept(self.listen_socket) {
                        // The request has usually already arrived (TCP_DEFER_ACCEPT), so try serving it before
                        // involving epoll at all
                        let conn = self.new_connection(ip);
                        self.advance(fd, conn);
                    }
                } else {
                    let fd = event.u64 as NativeSocket;
                    if let Some(conn) = self.connections.remove(&fd) {
                        self.advance(fd, conn);
                    }
                }
            }

            if self.is_rescue() && last_rescue.elapsed() >= RESCUE_INTERVAL {
                self.rescue();
                last_rescue = Instant::now();
            }

            if !self.connections.is_empty() && last_sweep.elapsed() >= SWEEP_INTERVAL {
                self.close_idle();
                last_sweep = Instant::now();
            }
        }
    }

    // Serves the connections waiting on any worker's listening socket while that worker has made no progress since
    // the last check
    // Workers run under SCHED_IDLE, so one sharing its CPU with a busy unrelated thread barely runs, and a handler
    // that takes a long time also holds up everything queued behind it
    // This thread has a normal priority and no fixed CPU, so those connections wait at most about RESCUE_INTERVAL
    fn rescue(&mut self) {
        let mut ready: Vec<libc::pollfd> = self.watched.iter().map(|w| libc::pollfd { fd: w.socket, events: libc::POLLIN, revents: 0 }).collect();
        let polled = unsafe { libc::poll(ready.as_mut_ptr(), ready.len() as libc::nfds_t, 0) };
        if polled < 0 {
            log_os_error("poll");
        }
        for (i, poll) in ready.iter().enumerate() {
            let watched = &mut self.watched[i];
            let progress = watched.progress.0.load(Ordering::Relaxed);
            let stalled = progress == watched.seen;
            watched.seen = progress;
            if polled > 0 && stalled && poll.revents & libc::POLLIN != 0 {
                let socket = watched.socket;
                // The socket may be blocking (see set_listener_in_epoll), so check there is still a connection
                // waiting before each accept, and a worker that wakes and takes it first leaves this waiting at
                // most ACCEPT_TIMEOUT
                while let Some((fd, ip)) = self.listener.accept(socket) {
                    let conn = self.new_connection(ip);
                    self.advance(fd, conn);
                    let mut again = libc::pollfd { fd: socket, events: libc::POLLIN, revents: 0 };
                    if unsafe { libc::poll(&mut again, 1, 0) } <= 0 || again.revents & libc::POLLIN == 0 {
                        break;
                    }
                }
            }
        }
    }

    // Drives conn and then either closes it or keeps it, registered for what it is now waiting on
    fn advance(&mut self, fd: NativeSocket, mut conn: Connection<'a>) {
        let interest = match self.listener.drive(fd, &mut conn) {
            Next::Close => {
                // Closing also removes it from epoll
                close(fd);
                if self.spare.capacity() == 0 && conn.request.capacity() <= RECV_BYTES {
                    conn.request.clear();
                    self.spare = conn.request;
                }
                return;
            }
            Next::Wait(interest) => interest,
        };
        if interest != conn.interest {
            let op = if conn.interest == Interest::None { libc::EPOLL_CTL_ADD } else { libc::EPOLL_CTL_MOD };
            if let Err(e) = epoll_ctl(self.epoll, op, fd, interest.events(), fd as u64) {
                log_error!("epoll_ctl: {}", e);
                close(fd);
                return;
            }
            conn.interest = interest;
        }
        self.connections.insert(fd, conn);
    }

    // Closes connections that have gone IDLE_TIMEOUT without making progress
    // Browsers open spare "preconnect" sockets they may never send a request on, so these are expected
    fn close_idle(&mut self) {
        self.connections.retain(|&fd, conn| {
            let keep = conn.last_active.elapsed() < IDLE_TIMEOUT;
            if !keep {
                log_info!("Closing connection idle for {:?} after receiving {} bytes", IDLE_TIMEOUT, conn.request.len());
                close(fd);
            }
            keep
        });
    }
}

impl<C> Drop for Worker<'_, '_, '_, C> {
    fn drop(&mut self) {
        for &fd in self.connections.keys() {
            close(fd);
        }
        unsafe { libc::close(self.epoll) };
        if self.owns_listen_socket {
            close(self.listen_socket);
        }
    }
}

// C must be Sync because the workers all share the context
impl<C: Sync> TcpListener<'_, C> {
    pub fn run(&self) -> bool {
        // Start listening
        if self.socket < 0 || self.port < 0 {
            return false;
        }
        if let Err(e) = start_listening(self.socket) {
            log_error!("listen: {}", e);
            return false;
        }
        log_info!("Now serving on port {}", self.port);

        // A fixed pool of workers, one per core, each multiplexing many connections with its own epoll instance
        // Spawning a thread for every connection cost more than serving the request, and a connection that is slow
        // or never sends anything (like a browser's preconnect socket) only takes up a map entry, not a thread
        // Each worker gets its own listening socket on the port, and the kernel spreads new connections across them,
        // so workers never contend over one accept queue
        let mut sockets = Vec::new();
        for (i, cpu) in worker_cpus().into_iter().enumerate() {
            let (listen_socket, owned) = if i == 0 {
                (self.socket, false)
            } else {
                match open_listener(self.port) {
                    Ok(socket) => (socket, true),
                    Err(e) => {
                        log_error!("Failed to open a listening socket for worker {}: {}", i, e);
                        continue;
                    }
                }
            };
            if let Some(cpu) = cpu {
                if let Err(e) = steer_to_cpu(listen_socket, cpu) {
                    log_error!("Failed to steer connections on CPU {} to its worker: {}", cpu, e);
                }
            }
            sockets.push((listen_socket, owned, cpu));
        }
        let progress: Vec<Progress> = sockets.iter().map(|_| Progress::default()).collect();

        // Scoped threads can borrow self (and the context) since they are all joined before run returns
        thread::scope(|scope| {
            for (&(listen_socket, owned, cpu), progress) in sockets.iter().zip(&progress) {
                scope.spawn(move || {
                    if let Some(cpu) = cpu {
                        if let Err(e) = pin_to_cpu(cpu) {
                            log_error!("Failed to pin the worker for CPU {}: {}", cpu, e);
                        } else if let Err(e) = deprioritize_worker() {
                            // Only pinned workers benefit, since the point is keeping the client on the worker's CPU
                            log_error!("Failed to run the worker for CPU {} under SCHED_IDLE: {}", cpu, e);
                        }
                    }
                    if let Some(mut worker) = Worker::new(self, listen_socket, owned) {
                        worker.progress = Some(progress);
                        worker.run();
                    } else if owned {
                        close(listen_socket);
                    }
                });
            }

            // The calling thread becomes the rescue thread, at its own (normal) priority
            if let Some(mut rescue) = Worker::new(self, INVALID_SOCKET, false) {
                rescue.watched = sockets
                    .iter()
                    .zip(&progress)
                    .map(|(&(socket, _, _), progress)| Watched { socket, progress, seen: 0 })
                    .collect();
                rescue.run();
            }
        });
        // The workers only return if they could not start
        false
    }
}

impl<C> Drop for TcpListener<'_, C> {
    fn drop(&mut self) {
        if self.socket >= 0 {
            unsafe { libc::close(self.socket) };
        }
    }
}
