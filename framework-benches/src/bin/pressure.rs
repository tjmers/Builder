// How the servers behave beyond peak requests/s: tail latency, memory, and connection scaling
// usage: pressure <latency|memory|scaling> [--duration SECONDS] [--servers builder,axum,...]
// Prints a Markdown table and writes it to results/<subcommand>.md
//
// The client runs on tokio so 10k connections don't take 10k client threads, and every request is on a new connection
// with Connection: close, like the wire benches

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use framework_benches::{client_cpu_count, get_request, pin_client, Framework, ServerProc};
use hdrhistogram::Histogram;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;

// A request taking longer than this counts as an error
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// The open-loop client stops sending past this many unfinished requests, and counts each one it skips as an error
const MAX_IN_FLIGHT: usize = 10_000;
// The histograms are split so tasks rarely wait on each other to record a result
const SHARDS: usize = 64;

const PEAK_CLIENTS: usize = 16;
const LATENCY_FRACTIONS: [f64; 3] = [0.25, 0.50, 0.75];
const HELD_CONNECTIONS: [usize; 2] = [100, 1000];
const CONCURRENCY_LEVELS: [usize; 4] = [100, 1000, 5000, 10_000];
const MEMORY_LOAD_CLIENTS: usize = 16;

// ----- Recording results -----

struct Stats {
    // Microseconds
    latency: Histogram<u64>,
    errors: BTreeMap<String, u64>,
}

impl Stats {
    fn new() -> Self {
        Stats {
            latency: Histogram::new_with_bounds(1, 120_000_000, 3).expect("histogram bounds"),
            errors: BTreeMap::new(),
        }
    }

    fn merge(&mut self, other: &Stats) {
        self.latency.add(&other.latency).expect("merge histograms");
        for (error, count) in &other.errors {
            *self.errors.entry(error.clone()).or_default() += count;
        }
    }

    fn ok(&self) -> u64 {
        self.latency.len()
    }

    fn millis(&self, quantile: f64) -> f64 {
        self.latency.value_at_quantile(quantile) as f64 / 1000.0
    }

    fn max_millis(&self) -> f64 {
        self.latency.max() as f64 / 1000.0
    }

    fn errors_cell(&self) -> String {
        if self.errors.is_empty() {
            return "0".to_owned();
        }
        self.errors.iter().map(|(error, count)| format!("{count} {error}")).collect::<Vec<_>>().join(", ")
    }
}

struct Recorder {
    shards: Vec<Mutex<Stats>>,
}

impl Recorder {
    fn new() -> Arc<Self> {
        Arc::new(Recorder { shards: (0..SHARDS).map(|_| Mutex::new(Stats::new())).collect() })
    }

    fn record(&self, shard: usize, result: Result<Duration, String>) {
        let mut stats = self.shards[shard % SHARDS].lock().expect("recorder lock");
        match result {
            Ok(latency) => stats.latency.saturating_record(latency.as_micros() as u64),
            Err(error) => *stats.errors.entry(error).or_default() += 1,
        }
    }

    fn total(&self) -> Stats {
        let mut total = Stats::new();
        for shard in &self.shards {
            total.merge(&shard.lock().expect("recorder lock"));
        }
        total
    }
}

// ----- The client -----

async fn request_once(port: u16, request: &[u8]) -> Result<(), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| format!("connect {:?}", e.kind()))?;
    stream.write_all(request).await.map_err(|e| format!("write {:?}", e.kind()))?;

    let mut chunk = [0u8; 4096];
    let mut got_response = false;
    loop {
        let n = stream.read(&mut chunk).await.map_err(|e| format!("read {:?}", e.kind()))?;
        if n == 0 {
            break;
        }
        if !got_response && !chunk[..n].starts_with(b"HTTP/1.1 200") {
            return Err("non-200 response".to_owned());
        }
        got_response = true;
    }
    if got_response {
        Ok(())
    } else {
        Err("closed without a response".to_owned())
    }
}

async fn timed_request(port: u16, request: &[u8]) -> Result<(), String> {
    tokio::time::timeout(REQUEST_TIMEOUT, request_once(port, request))
        .await
        .unwrap_or_else(|_| Err("timeout".to_owned()))
}

// clients tasks each send requests one after another until the time is up
fn closed_loop(runtime: &Runtime, port: u16, request: &[u8], clients: usize, duration: Duration) -> (Stats, Duration) {
    let request: Arc<[u8]> = request.into();
    let recorder = Recorder::new();
    let start = Instant::now();
    let deadline = start + duration;

    let tasks: Vec<_> = (0..clients)
        .map(|client| {
            let (request, recorder) = (Arc::clone(&request), Arc::clone(&recorder));
            runtime.spawn(async move {
                while Instant::now() < deadline {
                    let sent = Instant::now();
                    let result = timed_request(port, &request).await;
                    recorder.record(client, result.map(|()| sent.elapsed()));
                }
            })
        })
        .collect();
    runtime.block_on(async {
        for task in tasks {
            task.await.expect("client task");
        }
    });
    (recorder.total(), start.elapsed())
}

// Sends at a fixed rate whatever the server is doing, and measures each request from when it was scheduled to go out
// so a server that stalls can't hide the delay by holding up the requests behind it
// This thread spins to do the pacing since tokio's timers only have millisecond precision
// Also returns how late this thread ever was to send, because a stall here counts against the server
fn open_loop(runtime: &Runtime, port: u16, request: &[u8], rate: f64, duration: Duration) -> (Stats, Duration) {
    let request: Arc<[u8]> = request.into();
    let recorder = Recorder::new();
    let in_flight = Arc::new(AtomicUsize::new(0));

    let total = (rate * duration.as_secs_f64()) as u64;
    let start = Instant::now();
    let mut max_lag = Duration::ZERO;
    for i in 0..total {
        let scheduled = start + Duration::from_secs_f64(i as f64 / rate);
        while Instant::now() < scheduled {
            std::hint::spin_loop();
        }
        max_lag = max_lag.max(scheduled.elapsed());

        if in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
            recorder.record(i as usize, Err("skipped, too many in flight".to_owned()));
            continue;
        }
        in_flight.fetch_add(1, Ordering::Relaxed);
        let (request, recorder, in_flight) = (Arc::clone(&request), Arc::clone(&recorder), Arc::clone(&in_flight));
        runtime.spawn(async move {
            let result = timed_request(port, &request).await;
            recorder.record(i as usize, result.map(|()| scheduled.elapsed()));
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }

    // Each request ends within REQUEST_TIMEOUT
    while in_flight.load(Ordering::Relaxed) > 0 {
        thread::sleep(Duration::from_millis(10));
    }
    (recorder.total(), max_lag)
}

// Opens n connections that each hold an unfinished request, so the server has to keep them open
fn hold_connections(runtime: &Runtime, port: u16, n: usize) -> Vec<TcpStream> {
    runtime.block_on(async {
        let mut held = Vec::with_capacity(n);
        for _ in 0..n {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            stream.write_all(b"GET /plaintext HTTP/1.1\r\n").await.expect("write");
            held.push(stream);
        }
        held
    })
}

// How many of these connections the server hasn't closed
fn count_open(held: &[TcpStream]) -> usize {
    let mut byte = 0u8;
    held.iter()
        .filter(|stream| {
            // Asks the kernel directly: with nothing to read, an open socket has no data yet rather than an EOF
            let flags = libc::MSG_PEEK | libc::MSG_DONTWAIT;
            let n = unsafe { libc::recv(stream.as_raw_fd(), (&raw mut byte).cast(), 1, flags) };
            n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
        })
        .count()
}

// ----- The subcommands -----

fn exit_note(server: &mut ServerProc) -> Option<String> {
    server.exited().map(|status| format!("server exited ({status})"))
}

fn latency(runtime: &Runtime, frameworks: &[Framework], duration: Duration) -> String {
    let request = get_request("/json", true);

    let mut peaks = Vec::new();
    for &framework in frameworks {
        let server = ServerProc::spawn(framework);
        let (stats, elapsed) = closed_loop(runtime, server.port(), &request, PEAK_CLIENTS, duration / 2);
        let peak = stats.ok() as f64 / elapsed.as_secs_f64();
        eprintln!("{}: peak {peak:.0} req/s, errors {}", framework.name(), stats.errors_cell());
        peaks.push((framework, peak));
    }
    let slowest = peaks.iter().map(|(_, peak)| *peak).fold(f64::INFINITY, f64::min);

    let mut table = format!(
        "## Tail latency\n\n`GET /json`, open loop, measured from each request's scheduled send time. Peak is {PEAK_CLIENTS} closed-loop clients. \
         Client lag is how late the client's send loop ever ran: a stall that long is the client or the machine, not the server.\n\n\
         | Server | Peak req/s | Load | Rate req/s | OK | p50 ms | p99 ms | p99.9 ms | Max ms | Client lag ms | Errors |\n\
         |---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for (framework, peak) in peaks {
        let mut server = ServerProc::spawn(framework);
        let shared = ("shared (50% of slowest)".to_owned(), slowest * 0.5);
        let own = LATENCY_FRACTIONS.map(|fraction| (format!("{:.0}% of own peak", fraction * 100.0), peak * fraction));
        for (load, rate) in std::iter::once(shared).chain(own) {
            let (stats, lag) = open_loop(runtime, server.port(), &request, rate.max(1.0), duration);
            let lag_ms = lag.as_secs_f64() * 1000.0;
            let note = exit_note(&mut server).map_or(String::new(), |note| format!(", {note}"));
            let errors = stats.errors_cell();
            eprintln!("{} at {load}: p99 {:.2}ms, client lag {lag_ms:.2}ms, errors {errors}{note}", framework.name(), stats.millis(0.99));
            writeln!(
                table,
                "| {} | {peak:.0} | {load} | {rate:.0} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {lag_ms:.2} | {errors}{note} |",
                framework.name(),
                stats.ok(),
                stats.millis(0.50),
                stats.millis(0.99),
                stats.millis(0.999),
                stats.max_millis(),
            )
            .expect("write to string");
            if note.is_empty() {
                thread::sleep(Duration::from_secs(1));
            } else {
                break;
            }
        }
    }
    table
}

fn memory(runtime: &Runtime, frameworks: &[Framework], duration: Duration) -> String {
    let mut held_table = String::from(
        "## Memory while holding connections\n\nEach connection holds an unfinished request, sampled about 2s after the last one connected. \
         RSS excludes kernel thread stacks and socket buffers, so it undercounts thread-per-connection servers; compare thread counts too.\n\n\
         | Server | Connections | Still open | Idle RSS KB | Held RSS KB | KB per connection | Idle threads | Held threads |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    let mut load_table = format!(
        "\n## Memory after load\n\n{MEMORY_LOAD_CLIENTS} closed-loop clients on `GET /json` for {}s.\n\n\
         | Server | Requests | Errors | RSS after KB | Peak RSS KB |\n|---|---|---|---|---|\n",
        duration.as_secs()
    );

    for &framework in frameworks {
        for connections in HELD_CONNECTIONS {
            // A new process each time, so what the allocator kept from an earlier round doesn't count
            let server = ServerProc::spawn(framework);
            thread::sleep(Duration::from_secs(1));
            let idle = server.status();

            let held = hold_connections(runtime, server.port(), connections);
            thread::sleep(Duration::from_secs(2));
            let loaded = server.status();
            let open = count_open(&held);
            let per_connection = loaded.rss_kb.saturating_sub(idle.rss_kb) as f64 / connections as f64;
            eprintln!("{} holding {connections}: {} KB RSS, {} threads", framework.name(), loaded.rss_kb, loaded.threads);
            writeln!(
                held_table,
                "| {} | {connections} | {open} | {} | {} | {per_connection:.1} | {} | {} |",
                framework.name(),
                idle.rss_kb,
                loaded.rss_kb,
                idle.threads,
                loaded.threads,
            )
            .expect("write to string");
        }

        let mut server = ServerProc::spawn(framework);
        let (stats, _) = closed_loop(runtime, server.port(), &get_request("/json", true), MEMORY_LOAD_CLIENTS, duration);
        let after = server.status();
        let note = exit_note(&mut server).map_or(String::new(), |note| format!(", {note}"));
        writeln!(
            load_table,
            "| {} | {} | {}{note} | {} | {} |",
            framework.name(),
            stats.ok(),
            stats.errors_cell(),
            after.rss_kb,
            after.peak_rss_kb,
        )
        .expect("write to string");
    }
    held_table + &load_table
}

fn scaling(runtime: &Runtime, frameworks: &[Framework], duration: Duration) -> String {
    let request = get_request("/plaintext", true);
    let mut table = format!(
        "## Connection scaling\n\n`GET /plaintext`, closed loop, {}s per level, a new server process for each level. Peak threads is the most seen polling every 100ms.\n\n\
         | Server | Clients | Req/s | p50 ms | p99 ms | Peak threads | Peak RSS KB | Errors |\n\
         |---|---|---|---|---|---|---|---|\n",
        duration.as_secs()
    );
    for &framework in frameworks {
        for clients in CONCURRENCY_LEVELS {
            let mut server = ServerProc::spawn(framework);
            let port = server.port();

            // Polled during the run, since a server's threads may only last as long as a request
            let (result, peak_threads) = thread::scope(|scope| {
                let client = scope.spawn(|| closed_loop(runtime, port, &request, clients, duration));
                let mut peak_threads = 0;
                while !client.is_finished() {
                    peak_threads = peak_threads.max(server.status().threads);
                    thread::sleep(Duration::from_millis(100));
                }
                (client.join().expect("client thread"), peak_threads)
            });
            let (stats, elapsed) = result;
            let after = server.status();
            let note = exit_note(&mut server);
            let suffix = note.as_ref().map_or(String::new(), |note| format!(", {note}"));
            let rate = stats.ok() as f64 / elapsed.as_secs_f64();
            eprintln!("{} at {clients}: {rate:.0} req/s, errors {}{suffix}", framework.name(), stats.errors_cell());
            writeln!(
                table,
                "| {} | {clients} | {rate:.0} | {:.2} | {:.2} | {} | {} | {}{suffix} |",
                framework.name(),
                stats.millis(0.50),
                stats.millis(0.99),
                peak_threads,
                after.peak_rss_kb,
                stats.errors_cell(),
            )
            .expect("write to string");

            // A server that fell over at this level won't do better at the next one
            if note.is_some() {
                break;
            }
        }
    }
    table
}

// ----- Arguments -----

struct Args {
    subcommand: String,
    duration: Option<Duration>,
    frameworks: Vec<Framework>,
}

fn parse_args() -> Args {
    const USAGE: &str = "usage: pressure <latency|memory|scaling> [--duration SECONDS] [--servers builder,axum,...]";
    let mut args = std::env::args().skip(1);
    let subcommand = args.next().expect(USAGE);
    let mut parsed = Args { subcommand, duration: None, frameworks: Framework::ALL.to_vec() };
    while let Some(flag) = args.next() {
        let value = args.next().expect(USAGE);
        match flag.as_str() {
            "--duration" => parsed.duration = Some(Duration::from_secs(value.parse().expect(USAGE))),
            "--servers" => {
                parsed.frameworks = value
                    .split(',')
                    .map(|name| {
                        *Framework::ALL
                            .iter()
                            .find(|framework| framework.name() == name)
                            .unwrap_or_else(|| panic!("unknown server {name}"))
                    })
                    .collect();
            }
            _ => panic!("{USAGE}"),
        }
    }
    parsed
}

fn main() {
    let args = parse_args();
    pin_client();
    // One client CPU is left for open_loop's pacing thread
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads((client_cpu_count() - 1).max(1))
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let seconds = |default| args.duration.unwrap_or(Duration::from_secs(default));
    let report = match args.subcommand.as_str() {
        "latency" => latency(&runtime, &args.frameworks, seconds(20)),
        "memory" => memory(&runtime, &args.frameworks, seconds(30)),
        "scaling" => scaling(&runtime, &args.frameworks, seconds(20)),
        other => panic!("unknown subcommand {other}"),
    };

    println!("{report}");
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/results");
    std::fs::create_dir_all(directory).expect("create the results directory");
    std::fs::write(format!("{directory}/{}.md", args.subcommand), report).expect("write the results");

    // Don't wait for tasks still running on the runtime
    runtime.shutdown_background();
}
