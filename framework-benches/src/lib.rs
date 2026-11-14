// What the server binaries and the benches share: the workloads, starting and pinning a server process,
// and the blocking client (adapted from the builder crate's benches/common/mod.rs on the development branch)

use std::hint::black_box;
use std::io::{self, Read, Write};
use std::mem;
use std::net::{TcpListener, TcpStream};
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

pub const PLAINTEXT: &str = "Hello, World!";

pub const POST_BODY_BYTES: usize = 4096;
pub static POST_BODY: [u8; POST_BODY_BYTES] = [b'a'; POST_BODY_BYTES];

// The listen backlog, for the servers that let it be set
pub const BACKLOG: u32 = 4096;

#[derive(Serialize)]
pub struct JsonMessage {
    pub message: &'static str,
}

pub const JSON_MESSAGE: JsonMessage = JsonMessage { message: PLAINTEXT };

// The length of the body GET /json answers with
pub fn json_body_len() -> usize {
    serde_json::to_vec(&JSON_MESSAGE).expect("serialize").len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framework {
    Builder,
    Std,
    Axum,
    Actix,
    TinyHttp,
}

impl Framework {
    pub const ALL: [Framework; 5] = [
        Framework::Builder,
        Framework::Std,
        Framework::Axum,
        Framework::Actix,
        Framework::TinyHttp,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Framework::Builder => "builder",
            Framework::Std => "std",
            Framework::Axum => "axum",
            Framework::Actix => "actix",
            Framework::TinyHttp => "tiny_http",
        }
    }

    // Outside the 381xx range of the builder tests and the 382xx range of its benches
    pub fn port(self) -> u16 {
        38300 + self as u16
    }
}

// The port a server binary was told to serve on
pub fn port_arg() -> u16 {
    std::env::args()
        .nth(1)
        .and_then(|port| port.parse().ok())
        .expect("usage: server_<name> <port>")
}

// A listener with the same backlog as the other servers, which std doesn't expose (it uses 128)
// Calling listen again on a listening socket just updates its backlog
pub fn std_listener(port: u16) -> TcpListener {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    if unsafe { libc::listen(listener.as_raw_fd(), BACKLOG as i32) } < 0 {
        panic!("listen: {}", io::Error::last_os_error());
    }
    listener
}

fn cpu_count() -> usize {
    // Not available_parallelism, which shrinks once this process has been pinned
    let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    usize::try_from(online).unwrap_or(2).max(2)
}

// The server gets the first half of the CPUs and the client the second, so neither takes CPU time from the other
fn server_cpus() -> Range<usize> {
    0..cpu_count() / 2
}

fn client_cpus() -> Range<usize> {
    cpu_count() / 2..cpu_count()
}

// How many threads the client can run without oversubscribing its CPUs
pub fn client_cpu_count() -> usize {
    client_cpus().len()
}

fn set_affinity(cpus: Range<usize>) -> io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        for cpu in cpus {
            libc::CPU_SET(cpu, &mut set);
        }
        if libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// Pins the calling thread, and so every thread it starts from now on, to the client's CPUs
pub fn pin_client() {
    set_affinity(client_cpus()).expect("pin the client");
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcStatus {
    pub rss_kb: u64,
    pub peak_rss_kb: u64,
    pub threads: u64,
}

// A server running as its own process, killed when this is dropped
pub struct ServerProc {
    child: Child,
    framework: Framework,
}

// The server binaries sit next to the running one, which may be in deps/ if it is a bench
fn server_path(framework: Framework) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let mut dir = exe.parent().expect("exe directory");
    if dir.ends_with("deps") {
        dir = dir.parent().expect("deps parent");
    }
    let path = dir.join(format!("server_{}", framework.name()));
    assert!(path.exists(), "{} is not built, run cargo build --release --bins", path.display());
    path
}

impl ServerProc {
    // Starts the server and waits until it accepts connections
    pub fn spawn(framework: Framework) -> Self {
        let cpus = server_cpus();
        let mut command = Command::new(server_path(framework));
        command.arg(framework.port().to_string()).stdout(Stdio::null());
        unsafe {
            command.pre_exec(move || set_affinity(cpus.clone()));
        }
        let child = command.spawn().expect("start the server");

        let mut server = ServerProc { child, framework };
        server.wait_until_listening();
        server
    }

    fn wait_until_listening(&mut self) {
        for _ in 0..100 {
            if let Some(status) = self.exited() {
                panic!("{} exited while starting: {status}", self.framework.name());
            }
            if TcpStream::connect(("127.0.0.1", self.framework.port())).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("{} never started listening", self.framework.name());
    }

    pub fn port(&self) -> u16 {
        self.framework.port()
    }

    // The exit status if the server has died
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("check on the server")
    }

    // Memory and thread count from /proc/<pid>/status
    pub fn status(&self) -> ProcStatus {
        let path = format!("/proc/{}/status", self.child.id());
        let mut status = ProcStatus::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return status;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let value = value.trim().trim_end_matches(" kB").parse().unwrap_or(0);
            match key {
                "VmRSS" => status.rss_kb = value,
                "VmHWM" => status.peak_rss_kb = value,
                "Threads" => status.threads = value,
                _ => {}
            }
        }
        status
    }
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// connection_close is false only for the keep-alive reference numbers
pub fn get_request(path: &str, connection_close: bool) -> Vec<u8> {
    let connection = if connection_close { "Connection: close\r\n" } else { "" };
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{connection}\r\n").into_bytes()
}

// A POST /post with a POST_BODY_BYTES body
pub fn post_request() -> Vec<u8> {
    let mut request =
        format!("POST /post HTTP/1.1\r\nHost: localhost\r\nContent-Length: {POST_BODY_BYTES}\r\nConnection: close\r\n\r\n")
            .into_bytes();
    request.extend_from_slice(&POST_BODY);
    request
}

// Sends request on a new connection and reads the whole response into buf, returning its length
pub fn round_trip(port: u16, request: &[u8], buf: &mut Vec<u8>) -> usize {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.write_all(request).expect("write request");
    buf.clear();
    stream.read_to_end(buf).expect("read response");
    buf.len()
}

// Fails loudly if the server answers with anything but a 200 and a body of body_len bytes, or keeps the connection
// open, so a broken server can't look fast
pub fn check_response(port: u16, request: &[u8], body_len: usize) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("set read timeout");
    stream.write_all(request).expect("write request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("server did not close the connection after its response");

    assert!(
        response.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "response from port {port}: {:?}",
        String::from_utf8_lossy(&response)
    );
    let body_start = head_len(&response).expect("response has no end of headers");
    assert_eq!(response.len() - body_start, body_len, "unexpected response body length from port {port}");
}

// The index just past the blank line that ends the head
fn head_len(response: &[u8]) -> Option<usize> {
    response.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn content_length(head: &[u8]) -> Option<usize> {
    std::str::from_utf8(head)
        .ok()?
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

// Sends request on an open connection and reads one response into buf, using its Content-Length to find the end
fn keep_alive_round_trip(stream: &mut TcpStream, request: &[u8], buf: &mut Vec<u8>) -> usize {
    stream.write_all(request).expect("write request");
    buf.clear();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).expect("read response");
        assert!(n > 0, "server closed a keep-alive connection");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(head) = head_len(buf) {
            let total = head + content_length(&buf[..head]).expect("response has no Content-Length");
            if buf.len() >= total {
                return total;
            }
        }
    }
}

// Runs one_client's iterations split across clients threads, and returns the wall clock time they took
// Each thread's setup runs before the clock starts
fn run_split<T: Send>(
    clients: usize,
    iters: u64,
    setup: impl Fn() -> T + Sync,
    one_request: impl Fn(&mut T, &mut Vec<u8>) -> usize + Sync,
) -> Duration {
    let clients_u64 = clients as u64;
    // Everyone (the clients and this thread) waits here, so spawning threads is not measured
    let start_line = Barrier::new(clients + 1);
    thread::scope(|scope| {
        for i in 0..clients_u64 {
            // The first iters % clients clients take one extra request
            let share = iters / clients_u64 + u64::from(i < iters % clients_u64);
            let (start_line, setup, one_request) = (&start_line, &setup, &one_request);
            scope.spawn(move || {
                let mut state = setup();
                let mut buf = Vec::with_capacity(2 * POST_BODY_BYTES);
                start_line.wait();
                for _ in 0..share {
                    black_box(one_request(&mut state, &mut buf));
                }
            });
        }
        start_line.wait();
        let start = Instant::now();
        // Leaving the scope joins every client
        start
    })
    .elapsed()
}

// iters requests from clients threads, each request on a new connection
pub fn run_clients(port: u16, request: &[u8], clients: usize, iters: u64) -> Duration {
    run_split(clients, iters, || (), |_, buf| round_trip(port, black_box(request), buf))
}

// iters requests from clients threads, each thread reusing one connection
pub fn run_keep_alive_clients(port: u16, request: &[u8], clients: usize, iters: u64) -> Duration {
    run_split(
        clients,
        iters,
        || TcpStream::connect(("127.0.0.1", port)).expect("connect"),
        |stream, buf| keep_alive_round_trip(stream, black_box(request), buf),
    )
}
