use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::mem::{self, MaybeUninit};
use std::net::Ipv4Addr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use libc::{c_int, c_void, epoll_event, sockaddr, sockaddr_in, socklen_t};

use super::{log_closed_early, request_len, Framing, NativeSocket, RawRequestHandler, TcpListener, IDLE_TIMEOUT};
use crate::ip_address::IpAddress;

const INVALID_SOCKET: NativeSocket = -1;

// The fewest threads serving parked connections, however few CPUs there are, so a few slow handlers cannot take up all of them
const MIN_PARKED_WORKERS: usize = 4;

// How much each recv reads at most, enough for a typical request with a small body in one call
const RECV_BYTES: usize = 16 * 1024;

// How often parked connections are checked for ones that have gone idle
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

// A connection with a response left to send is closed after this long without the client taking any of it
// Longer than IDLE_TIMEOUT since a client that pauses partway through a download has not necessarily gone away, but
// still bounded so a client that stops reading cannot hold on to the response forever
const SEND_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

// How long to wait before accepting again when the process is out of file descriptors or memory
// The connection stays queued and accept fails immediately until something is freed, so retrying straight away would
// only spin a CPU while the threads that could free something wait for it
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

// How long the kernel holds a new connection back from accept while waiting for its first data (TCP_DEFER_ACCEPT)
// After this (one SYN-ACK retransmit) it is handed over anyway, so a connection that never sends anything still
// reaches a worker and is closed after IDLE_TIMEOUT
const DEFER_ACCEPT_SECS: c_int = 1;

// Responses up to this size are closed without TIME_WAIT (see bind_socket), larger ones get a normal close
// The fast close resets the connection once the client has acknowledged the whole response, and a client that
// discards unread data on a reset (Windows does) could lose the end of a large response it is still reading
const FAST_CLOSE_MAX_BYTES: usize = 64 * 1024;

// How often the rescue thread checks for workers that have stopped making progress (see Rescue)
const RESCUE_INTERVAL: Duration = Duration::from_millis(10);

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

// Binds socket to port on every interface
// The TCP options set here are inherited by every connection accepted from the socket
fn bind_socket(socket: NativeSocket, port: i32) -> io::Result<()> {
    set_option(socket, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
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

// The port socket is bound to, which is the one the OS picked if it was asked for port 0
fn bound_port(socket: NativeSocket) -> io::Result<i32> {
    let mut addr: sockaddr_in = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<sockaddr_in>() as socklen_t;
    if unsafe { libc::getsockname(socket, &mut addr as *mut sockaddr_in as *mut sockaddr, &mut len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u16::from_be(addr.sin_port) as i32)
}

// Starts listening on a bound socket
fn start_listening(socket: NativeSocket) -> io::Result<()> {
    if unsafe { libc::listen(socket, libc::SOMAXCONN) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// Whether a failed accept means the process has run out of something, rather than something being wrong with the
// one connection it was accepting
fn out_of_resources(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM))
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

// A further listening socket on a port the first socket is already listening on, sharing it through SO_REUSEPORT
// It stays blocking, since its worker waits in accept
fn open_listener(port: i32) -> io::Result<NativeSocket> {
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = set_option(socket, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1)
        .and_then(|()| bind_socket(socket, port))
        .and_then(|()| start_listening(socket));
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

// A connection from accept until it is closed
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
    // A large response gets a normal close with TIME_WAIT instead of the fast close set up in bind_socket
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

    // Whether it has gone too long without progress, allowing longer for a client to take a response than to send a request
    fn is_idle(&self) -> bool {
        let timeout = if self.response.is_some() { SEND_IDLE_TIMEOUT } else { IDLE_TIMEOUT };
        self.last_active.elapsed() >= timeout
    }
}

impl<'a, C> TcpListener<'a, C> {
    pub fn new(context: &'a C, handler: RawRequestHandler<'a, C>) -> Self {
        // Create the socket but do not start listening
        // It stays blocking, since its worker waits in accept
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
        // This socket has no SO_REUSEPORT yet (see run), so it cannot join a server already listening on the port
        let bound = bind_socket(self.socket, port).and_then(|()| bound_port(self.socket));
        match bound {
            Ok(port) => {
                self.port = port;
                true
            }
            Err(e) => {
                log_error!("bind: {}", e);
                unsafe { libc::close(self.socket) };
                self.socket = INVALID_SOCKET;
                false
            }
        }
    }

    pub fn valid(&self) -> bool {
        self.socket >= 0
    }

    // Accepts one connection from a listening socket, waiting for one if the socket is blocking
    fn accept(&self, listen_socket: NativeSocket) -> io::Result<(NativeSocket, IpAddress)> {
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
            return Err(io::Error::last_os_error());
        }

        let ip = Ipv4Addr::from(u32::from_be(client.sin_addr.s_addr));
        log_info!("Connection from {}:{}", ip, u16::from_be(client.sin_port));
        Ok((conn, IpAddress::from(ip.octets())))
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

// What the threads share
// A connection normally goes from accept to close on its CPU's worker without touching any of this
// Only one that has to wait on its socket is parked here, where any parked worker can pick it up, so a slow handler
// never holds up a connection other than its own
struct Shared<'l, 'a, C> {
    listener: &'l TcpListener<'a, C>,
    // Wakes the parked workers when a parked connection's socket is ready
    epoll: c_int,
    // The connections waiting on their sockets
    // One being driven is taken out, so it belongs to one thread at a time and the idle sweep cannot close it
    parked: Mutex<HashMap<NativeSocket, Connection<'a>>>,
}

impl<'a, C> Shared<'_, 'a, C> {
    fn parked(&self) -> MutexGuard<'_, HashMap<NativeSocket, Connection<'a>>> {
        // Nothing panics while holding the lock, and the map is valid even if something did
        self.parked.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Parks conn until its socket is ready for interest
    fn park(&self, fd: NativeSocket, mut conn: Connection<'a>, interest: Interest) {
        let op = if conn.interest == Interest::None { libc::EPOLL_CTL_ADD } else { libc::EPOLL_CTL_MOD };
        conn.interest = interest;

        // A parked worker can have the event before epoll_ctl even returns, so the connection goes in the map first
        // Holding the lock across both stops the sweep closing this fd in between, so epoll_ctl cannot reach a new
        // connection that was given the same fd
        // EPOLLONESHOT has epoll report it once and then wait to be armed again here, so one thread has it at a time
        let mut parked = self.parked();
        parked.insert(fd, conn);
        let events = interest.events() | libc::EPOLLONESHOT as u32;
        if let Err(e) = epoll_ctl(self.epoll, op, fd, events, fd as u64) {
            log_error!("epoll_ctl: {}", e);
            parked.remove(&fd);
            close(fd);
        }
    }

    // Closes parked connections that have gone too long without making progress
    // Browsers open spare "preconnect" sockets they may never send a request on, so these are expected
    fn close_idle(&self) {
        self.parked().retain(|&fd, conn| {
            if !conn.is_idle() {
                return true;
            }
            match &conn.response {
                Some((response, sent)) => {
                    log_info!("Closing connection that stopped reading after {} of {} bytes", sent, response.len());
                }
                None => log_info!("Closing connection idle for {:?} after receiving {} bytes", IDLE_TIMEOUT, conn.request.len()),
            }
            // Closing also removes it from epoll
            close(fd);
            false
        });
    }
}

impl<C> Drop for Shared<'_, '_, C> {
    fn drop(&mut self) {
        for &fd in self.parked().keys() {
            close(fd);
        }
        unsafe { libc::close(self.epoll) };
    }
}

// Counts how many times a worker has come back from accept, so the rescue thread can tell when it has stalled
// Aligned to a cache line so workers bumping their own counters never contend
#[repr(align(64))]
#[derive(Default)]
struct Progress(AtomicU64);

impl Progress {
    fn bump(&self) {
        self.0.store(self.0.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }
}

// One worker thread, with a finished connection's receive buffer so most connections do not allocate one
struct Worker<'s, 'l, 'a, C> {
    shared: &'s Shared<'l, 'a, C>,
    spare: Vec<u8>,
}

impl<'a, C> Worker<'_, '_, 'a, C> {
    // Accepts connections from this worker's own socket and serves each one straight away
    // TCP_DEFER_ACCEPT means the request has usually already arrived, so most connections are accept, recv, send, close
    // Waiting in a blocking accept rather than in epoll saves a syscall per connection
    fn accept_connections(&mut self, socket: NativeSocket, progress: &Progress) {
        // Whether the last accept ran out of resources, so a shortage is logged once instead of on every retry
        let mut short = false;
        loop {
            let accepted = self.shared.listener.accept(socket);
            progress.bump();
            match accepted {
                Ok((fd, ip)) => {
                    short = false;
                    let conn = Connection::new(ip, mem::take(&mut self.spare));
                    self.advance(fd, conn);
                }
                Err(e) if out_of_resources(&e) => {
                    if !short {
                        log_error!("accept: {}, retrying until something is freed", e);
                        short = true;
                    }
                    thread::sleep(ACCEPT_BACKOFF);
                }
                // WouldBlock is the rescue thread having the socket non-blocking for a moment (see Rescue::take_over)
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                // If there is an error here do not shut down the server just continue
                Err(e) => log_error!("accept: {}", e),
            }
        }
    }

    // Serves parked connections as their sockets become ready, one at a time
    // Taking several events at once would leave the rest waiting behind this thread's handler while others sit idle
    fn serve_parked(&mut self) {
        let mut event = epoll_event { events: 0, u64: 0 };
        loop {
            let n = unsafe { libc::epoll_wait(self.shared.epoll, &mut event, 1, -1) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Nothing that fails here can recover, and carrying on would only spin
                log_error!("epoll_wait: {}", e);
                return;
            }
            if n == 0 {
                continue;
            }
            // The sweep can close a connection after its event was returned, which leaves nothing to take here
            // If its fd was reused meanwhile this takes the new connection, which finds nothing to do and parks again
            let fd = event.u64 as NativeSocket;
            let conn = self.shared.parked().remove(&fd);
            if let Some(conn) = conn {
                self.advance(fd, conn);
            }
        }
    }

    // Drives conn and then either closes it or parks it until its socket is ready for what it is waiting on
    fn advance(&mut self, fd: NativeSocket, mut conn: Connection<'a>) {
        match self.shared.listener.drive(fd, &mut conn) {
            Next::Close => {
                close(fd);
                if self.spare.capacity() == 0 && conn.request.capacity() <= RECV_BYTES {
                    conn.request.clear();
                    self.spare = conn.request;
                }
            }
            Next::Wait(interest) => self.shared.park(fd, conn, interest),
        }
    }
}

// A worker's listening socket, with the worker's progress at the last check
struct Watched<'p> {
    socket: NativeSocket,
    progress: &'p Progress,
    seen: u64,
}

// Takes over the connections queued on a worker's listening socket while that worker makes no progress, which is a
// long handler or a CPU busy with something else
// It hands them to the parked workers and never runs a handler itself, so nothing can hold it up, and those
// connections wait at most about twice RESCUE_INTERVAL
struct Rescue<'s, 'l, 'a, 'p, C> {
    shared: &'s Shared<'l, 'a, C>,
    watched: Vec<Watched<'p>>,
    // Whether the last accept ran out of resources, so a shortage is logged once instead of on every retry
    short: bool,
    // When to try again after running out of resources
    paused_until: Instant,
}

impl<C> Rescue<'_, '_, '_, '_, C> {
    fn run(&mut self) -> ! {
        let mut last_sweep = Instant::now();
        loop {
            thread::sleep(RESCUE_INTERVAL);
            if Instant::now() >= self.paused_until {
                self.check_workers();
            }
            if last_sweep.elapsed() >= SWEEP_INTERVAL {
                self.shared.close_idle();
                last_sweep = Instant::now();
            }
        }
    }

    fn check_workers(&mut self) {
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
                self.take_over(socket);
            }
        }
    }

    // Moves every connection queued on socket to the parked workers
    // The socket is non-blocking only for this, so this cannot get stuck if its worker wakes and takes them first
    // A worker already waiting in accept stays waiting, and one arriving meanwhile just tries again
    fn take_over(&mut self, socket: NativeSocket) {
        if let Err(e) = set_nonblocking(socket, true) {
            log_error!("Failed to make a stalled worker's socket non-blocking: {}", e);
            return;
        }
        loop {
            match self.shared.listener.accept(socket) {
                Ok((fd, ip)) => {
                    self.short = false;
                    self.shared.park(fd, Connection::new(ip, Vec::new()), Interest::Read);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    if !out_of_resources(&e) {
                        log_error!("accept: {}", e);
                    } else {
                        if !self.short {
                            log_error!("accept: {}, retrying until something is freed", e);
                            self.short = true;
                        }
                        self.paused_until = Instant::now() + ACCEPT_BACKOFF;
                    }
                    break;
                }
            }
        }
        if let Err(e) = set_nonblocking(socket, false) {
            log_error!("Failed to make a worker's socket blocking again: {}", e);
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
        // Without SO_REUSEPORT at this point, this fails if another socket is already listening on the port, even one
        // that was bound at the same time as this one, so two servers can never end up sharing the port
        if let Err(e) = start_listening(self.socket) {
            log_error!("listen: {}", e);
            return false;
        }
        // Now that this server has the port, let its workers' sockets join this one
        if let Err(e) = set_option(self.socket, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1) {
            log_error!("Failed to share the port between workers: {}", e);
            return false;
        }
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            log_os_error("epoll_create1");
            return false;
        }
        log_info!("Now serving on port {}", self.port);

        let shared = Shared { listener: self, epoll, parked: Mutex::new(HashMap::new()) };
        let shared = &shared;

        // A fixed pool of workers, one per core, since spawning a thread for every connection cost more than serving
        // the request
        // Each worker gets its own listening socket on the port, and the kernel spreads new connections across them,
        // so workers never contend over one accept queue
        let mut sockets = Vec::new();
        for (i, cpu) in worker_cpus().into_iter().enumerate() {
            let listen_socket = if i == 0 {
                self.socket
            } else {
                match open_listener(self.port) {
                    Ok(socket) => socket,
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
            sockets.push((listen_socket, cpu));
        }
        let progress: Vec<Progress> = sockets.iter().map(|_| Progress::default()).collect();
        // A connection that is slow or never sends anything (like a browser's preconnect socket) only takes up a map
        // entry here, not a thread
        // These are not pinned, so one is free to run wherever there is room
        let parked_workers = sockets.len().max(MIN_PARKED_WORKERS);

        // Scoped threads can borrow self (and the context) since they are all joined before run returns
        thread::scope(|scope| {
            for (&(listen_socket, cpu), progress) in sockets.iter().zip(&progress) {
                scope.spawn(move || {
                    if let Some(cpu) = cpu {
                        if let Err(e) = pin_to_cpu(cpu) {
                            log_error!("Failed to pin the worker for CPU {}: {}", cpu, e);
                        }
                    }
                    Worker { shared, spare: Vec::new() }.accept_connections(listen_socket, progress);
                });
            }
            for _ in 0..parked_workers {
                scope.spawn(move || Worker { shared, spare: Vec::new() }.serve_parked());
            }

            // The calling thread takes over from stalled workers and sweeps for idle connections
            let watched = sockets
                .iter()
                .zip(&progress)
                .map(|(&(socket, _), progress)| Watched { socket, progress, seen: 0 })
                .collect();
            Rescue { shared, watched, short: false, paused_until: Instant::now() }.run()
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
