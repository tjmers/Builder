use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::mem::{self, MaybeUninit};
use std::net::Ipv4Addr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use libc::{c_int, c_void, epoll_event, sockaddr, sockaddr_in, socklen_t};

use super::{log_closed_early, Framer, Framing, NativeSocket, RawRequestHandler, TcpListener, IDLE_TIMEOUT, SEND_IDLE_TIMEOUT};
use crate::ip_address::IpAddress;

const INVALID_SOCKET: NativeSocket = -1;

// The fewest threads serving parked connections, however few CPUs there are, so a few slow handlers cannot take up all of them
const MIN_PARKED_WORKERS: usize = 4;

// How much each recv reads at most, enough for a typical request with a small body in one call
const RECV_BYTES: usize = 16 * 1024;

// How often parked connections are checked for ones that have gone idle
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

// How long to wait before accepting again when the process is out of file descriptors or memory
// The connection stays queued and accept fails immediately until something is freed, so retrying straight away would
// only spin a CPU while the threads that could free something wait for it
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

// How long the kernel holds a new connection back from accept while waiting for its first data (TCP_DEFER_ACCEPT)
// After this (one SYN-ACK retransmit) it is handed over anyway, so a connection that never sends anything still
// reaches a worker and is closed after IDLE_TIMEOUT
const DEFER_ACCEPT_SECS: c_int = 1;

// How often the rescue thread checks for workers that have stopped making progress (see Rescue)
const RESCUE_INTERVAL: Duration = Duration::from_millis(10);

// How many checks in a row have to find nothing going on before the rescue thread stops checking and waits for the
// next connection to arrive instead, so a server with no traffic is not woken a hundred times a second
const RESCUE_QUIET_CHECKS: u32 = 10;

// The most connections the rescue thread takes from one stalled worker's socket in one check
// It then moves on to the other sockets and the idle sweep and comes back on the next check, so connections
// flooding in on one socket cannot keep it from everything else
const RESCUE_BATCH: usize = 64;

// The most receives one pass makes when discarding what a client sends while the end of its response goes out (see
// Connection::drain), so a client that keeps sending cannot keep a worker to itself
const DRAIN_RECVS: usize = 4;

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
    // No MSG_MORE: it would hold the end of a response back until close, so even the smallest response would have to
    // wait for the client to take it before the connection could be closed (see Connection::finish)
    let n = unsafe { libc::send(conn, data.as_ptr() as *const c_void, data.len(), libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn close(conn: NativeSocket) {
    unsafe { libc::close(conn) };
}

// Asks a connection's socket for one of its byte counts, or None if it could not say
fn queued_bytes(conn: NativeSocket, request: libc::Ioctl) -> Option<usize> {
    let mut bytes: c_int = 0;
    if unsafe { libc::ioctl(conn, request, &mut bytes) } < 0 {
        return None;
    }
    Some(bytes.max(0) as usize)
}

// How many of the bytes handed to send the client has yet to acknowledge (SIOCOUTQ, which is TIOCOUTQ by another name)
fn unacked(conn: NativeSocket) -> Option<usize> {
    queued_bytes(conn, libc::TIOCOUTQ)
}

// How many of the bytes handed to send have not gone out to the client at all yet
fn unsent(conn: NativeSocket) -> Option<usize> {
    queued_bytes(conn, libc::SIOCOUTQNSD as _)
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
    // TCP_LINGER2 is left alone, so the server closing first after every response leaves each connection in
    // TIME_WAIT as usual
    // A negative value would skip that, but only by answering the client's ACK of the FIN with a reset, and a
    // client or proxy that discards unread data on a reset could then lose a response it had already received

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

// Whether socket is ready for what a parked connection is waiting on, without waiting for it
fn is_ready(socket: NativeSocket, interest: Interest) -> bool {
    let events = match interest {
        Interest::None => return false,
        Interest::Read => libc::POLLIN,
        Interest::Write => libc::POLLOUT,
    };
    let mut poll = libc::pollfd { fd: socket, events, revents: 0 };
    unsafe { libc::poll(&mut poll, 1, 0) > 0 && poll.revents & events != 0 }
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
    // The request received so far, and how far along it is
    request: Vec<u8>,
    framer: Framer,
    // Once the request is handled, the response and how much of it has been sent
    response: Option<(Cow<'a, [u8]>, usize)>,
    // How much of the response the client had acknowledged at the last sweep
    acked: usize,
    // Whether the whole response has been handed to the kernel and the connection is only waiting for the client to
    // take the end of it (see finish)
    closing: bool,
    interest: Interest,
    // When the connection last made progress, for the idle timeout: the client sending some of its request or taking
    // some of its response
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
            framer: Framer::default(),
            response: None,
            acked: 0,
            closing: false,
            interest: Interest::None,
            last_active: Instant::now(),
        }
    }

    // Receives until the request is complete or the socket has nothing more for now
    // Returns the request's length once it is complete
    fn recv_request(&mut self, fd: NativeSocket) -> Result<Option<usize>, ()> {
        loop {
            match self.framer.advance(&self.request) {
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

    // Decides what to do once the whole response is in the kernel's send buffer
    // Closing a connection while the client has sent something that was never read (a pipelined request, a line
    // ending after a body) resets it, and a reset throws away whatever part of the response has not gone out yet
    // A response that has all gone out is safe from that, so the connection is closed straight away, which is every
    // response small enough to go out at once
    // Otherwise the client is told the response ends here, and the connection stays open, discarding whatever the
    // client sends, until the client has all of it
    fn finish(&mut self, fd: NativeSocket) -> Next {
        // If the kernel cannot say, close as if it had all gone out, which is what closing always did before
        if unsent(fd).unwrap_or(0) == 0 {
            return Next::Close;
        }
        unsafe { libc::shutdown(fd, libc::SHUT_WR) };
        self.closing = true;
        self.drain(fd)
    }

    // Discards what the client has sent since its request, so that closing cannot reset the connection, and closes
    // once the client has the whole response or has closed its own end
    // None of this counts as progress, so a client that keeps sending but takes none of the response is still closed
    // by the sweep
    fn drain(&mut self, fd: NativeSocket) -> Next {
        let mut discarded = [MaybeUninit::<u8>::uninit(); RECV_BYTES];
        for _ in 0..DRAIN_RECVS {
            match recv(fd, &mut discarded) {
                // The client will send nothing more, so there is nothing left that could go unread, and a close
                // from here on sends the rest of the response as usual
                Ok(0) => return Next::Close,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // The sweep closes it if the client takes the rest without sending or closing anything
                    return if unacked(fd).unwrap_or(0) == 0 { Next::Close } else { Next::Wait(Interest::Read) };
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Next::Close,
            }
        }
        // More is waiting, so this is picked up again straight away, after any other connection that is ready
        Next::Wait(Interest::Read)
    }

    // Whether it has gone too long without progress, allowing longer for a client to take a response than to send a request
    fn is_idle(&self) -> bool {
        let timeout = if self.response.is_some() { SEND_IDLE_TIMEOUT } else { IDLE_TIMEOUT };
        self.last_active.elapsed() >= timeout
    }

    // Whether the sweep should close it, while it is parked: it is finished, or the client has stopped
    fn expired(&mut self, fd: NativeSocket) -> bool {
        if let (Some((_, sent)), Some(unacked)) = (&self.response, unacked(fd)) {
            // A client taking its response slowly leaves the socket not ready to send for a long time at a stretch,
            // so its progress is what it has acknowledged, not what the socket has had room for
            let acked = sent.saturating_sub(unacked);
            if acked > self.acked {
                self.acked = acked;
                self.last_active = Instant::now();
            }
            if self.closing && unacked == 0 {
                return true;
            }
        }
        // One whose socket is ready is not idle: the client has done its part, and it is waiting for a parked
        // worker to come free, as it would have waited in the kernel's queue for accept
        self.is_idle() && !is_ready(fd, self.interest)
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
        if conn.closing {
            return conn.drain(fd);
        }
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
            conn.response = Some((response, 0));
        }

        match conn.send_response(fd) {
            Ok(true) => conn.finish(fd),
            Ok(false) => Next::Wait(Interest::Write),
            Err(()) => Next::Close,
        }
    }
}

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
fn steer_to_cpu(socket: NativeSocket, cpu: usize) -> io::Result<()> {
    set_option(socket, libc::SOL_SOCKET, libc::SO_INCOMING_CPU, cpu as c_int)
}

// What the threads share
// A connection normally goes from accept to close on its CPU's worker without touching any of this
// Only one that has to wait on its socket is parked here, where any parked worker can pick it up, so a slow handler
// never holds up a connection other than its own
// Nothing here is ever cleaned up, since run never returns: the epoll instance, the parked connections and the
// workers' listening sockets all last as long as the process
struct Shared<'l, 'a, C> {
    listener: &'l TcpListener<'a, C>,
    // Wakes the parked workers when a parked connection's socket is ready
    epoll: c_int,
    // The connections waiting on their sockets
    // One being driven is taken out, so it belongs to one thread at a time and the idle sweep cannot close it
    parked: Mutex<HashMap<NativeSocket, Connection<'a>>>,
    // How many parked workers are waiting for a connection, which is how many the rescue thread can hand one to
    free_workers: AtomicUsize,
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

    // Closes parked connections that are finished, or have gone too long without making progress
    // Browsers open spare "preconnect" sockets they may never send a request on, so these are expected
    fn close_idle(&self) {
        // They are taken out of the map with the lock held, and logged and closed once it is released, so a log that
        // is slow to write (a full pipe, say) does not hold up every thread that parks or takes a connection
        // Until it is closed here an fd cannot be given to a new connection, and a parked worker that already has
        // its event finds nothing in the map
        let expired: Vec<_> = self.parked().extract_if(|&fd, conn| conn.expired(fd)).collect();
        for (fd, conn) in expired {
            match &conn.response {
                Some((response, _)) if conn.acked < response.len() => {
                    log_info!("Closing connection that stopped reading after {} of {} bytes", conn.acked, response.len());
                }
                // The client has the whole response
                Some(_) => {}
                None => log_info!("Closing connection idle for {:?} after receiving {} bytes", IDLE_TIMEOUT, conn.request.len()),
            }
            // Closing also removes it from epoll
            close(fd);
        }
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
            self.shared.free_workers.fetch_add(1, Ordering::Relaxed);
            let n = unsafe { libc::epoll_wait(self.shared.epoll, &mut event, 1, -1) };
            self.shared.free_workers.fetch_sub(1, Ordering::Relaxed);
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
// It only takes as many as there are parked workers waiting for one
// The rest stay in the kernel's queue, where the stalled worker takes them itself if it comes back first, and where
// they take up no file descriptor and the kernel refuses new ones once it is full
struct Rescue<'s, 'l, 'a, 'p, C> {
    shared: &'s Shared<'l, 'a, C>,
    watched: Vec<Watched<'p>>,
    // One entry per watched socket, in the same order, kept between checks to save building it for each one
    ready: Vec<libc::pollfd>,
    // Whether the last accept ran out of resources, so a shortage is logged once instead of on every retry
    short: bool,
    // When to try again after running out of resources
    paused_until: Instant,
}

impl<'s, 'l, 'a, 'p, C> Rescue<'s, 'l, 'a, 'p, C> {
    fn new(shared: &'s Shared<'l, 'a, C>, watched: Vec<Watched<'p>>) -> Self {
        let ready = watched.iter().map(|w| libc::pollfd { fd: w.socket, events: libc::POLLIN, revents: 0 }).collect();
        Rescue { shared, watched, ready, short: false, paused_until: Instant::now() }
    }

    fn run(&mut self) -> ! {
        let mut next_sweep = Instant::now() + SWEEP_INTERVAL;
        // How many checks in a row found nothing going on
        let mut quiet = 0;
        loop {
            if quiet >= RESCUE_QUIET_CHECKS {
                // Nothing is going on, so sleep until a connection arrives on any worker's socket or the sweep is due
                // A connection its worker cannot get to leaves the socket readable, so this returns straight away
                // for as long as there is something to rescue
                self.poll_sockets(next_sweep.saturating_duration_since(Instant::now()));
            }
            thread::sleep(RESCUE_INTERVAL);
            let active = Instant::now() < self.paused_until || self.check_workers();
            quiet = if active { 0 } else { quiet.saturating_add(1) };

            if Instant::now() >= next_sweep {
                self.shared.close_idle();
                next_sweep = Instant::now() + SWEEP_INTERVAL;
            }
        }
    }

    // Finds which watched sockets have connections queued, waiting up to timeout for one to arrive
    // Returns false if none have
    fn poll_sockets(&mut self, timeout: Duration) -> bool {
        // Rounded up, so a wait for the sweep does not end just before it is due
        let millis = timeout.as_micros().div_ceil(1000).min(c_int::MAX as u128) as c_int;
        let polled = unsafe { libc::poll(self.ready.as_mut_ptr(), self.ready.len() as libc::nfds_t, millis) };
        if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            log_os_error("poll");
        }
        polled > 0
    }

    // Takes over from every worker that has made no progress since the last check while connections wait on its socket
    // Returns whether anything is going on: a worker made progress, or connections are waiting
    fn check_workers(&mut self) -> bool {
        let mut active = self.poll_sockets(Duration::ZERO);
        let waiting = active;
        // How many connections the parked workers can take right now, shared between every stalled socket
        // A worker that another connection wakes meanwhile makes this one too many, and that connection then waits
        // for the next worker to come free
        let mut free = self.shared.free_workers.load(Ordering::Relaxed);
        for i in 0..self.watched.len() {
            let watched = &mut self.watched[i];
            let progress = watched.progress.0.load(Ordering::Relaxed);
            let stalled = progress == watched.seen;
            watched.seen = progress;
            active |= !stalled;
            if waiting && stalled && self.ready[i].revents & libc::POLLIN != 0 {
                let socket = watched.socket;
                free -= self.take_over(socket, free.min(RESCUE_BATCH));
            }
        }
        active
    }

    // Moves the connections queued on socket to the parked workers, up to limit of them, and returns how many
    // The socket is non-blocking only for this, so this cannot get stuck if its worker wakes and takes them first
    // A worker already waiting in accept stays waiting, and one arriving meanwhile just tries again
    fn take_over(&mut self, socket: NativeSocket, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }
        if let Err(e) = set_nonblocking(socket, true) {
            log_error!("Failed to make a stalled worker's socket non-blocking: {}", e);
            return 0;
        }
        let mut taken = 0;
        while taken < limit {
            match self.shared.listener.accept(socket) {
                Ok((fd, ip)) => {
                    self.short = false;
                    taken += 1;
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
        taken
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

        let shared = Shared { listener: self, epoll, parked: Mutex::new(HashMap::new()), free_workers: AtomicUsize::new(0) };
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

        // Scoped threads can borrow self (and the context), since run never returns while they are running
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
            Rescue::new(shared, watched).run()
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
