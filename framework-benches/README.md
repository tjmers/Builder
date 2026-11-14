# framework-benches

Compares Builder with axum, actix-web, tiny_http, and a bare `std::net` server. This crate has its own workspace, so the Builder crate and its CI never compile these dependencies. Linux only.

## Running

Run from this directory. Close anything heavy first, especially compiles.

```sh
cargo bench --bench components                # parser and router, about 3 minutes
cargo bench --bench wire                      # throughput, about 20 minutes
cargo bench --bench wire -- wire_plaintext    # one group

cargo build --release --bins                  # pressure needs the servers built
./target/release/pressure latency             # tail latency, about 8 minutes
./target/release/pressure memory              # memory footprint, about 3 minutes
./target/release/pressure scaling             # connection scaling, about 8 minutes
```

`pressure` takes `--duration SECONDS` and `--servers builder,axum,...`, and writes its tables to `results/`. To check that everything works without measuring, use `cargo bench -- --test` and `pressure <subcommand> --duration 2`.

## What each benchmark measures

| Benchmark | Measures |
|---|---|
| `wire_plaintext` | `GET /plaintext`, a prebuilt 13-byte response, at 1 to 16 clients |
| `wire_post_4kb` | `POST /post`, reading a 4KB body and returning a prebuilt 4KB body |
| `wire_json` | `GET /json`, serializing with `serde_json` on every request |
| `wire_plaintext_keepalive_reference` | The other frameworks on reused connections. Reference only, since Builder can't run it |
| `pressure latency` | p50, p99, and p99.9 on `/json` at 25, 50, and 75% of each server's own peak, and at one shared rate |
| `pressure memory` | RSS and threads when idle, while holding 100 and 1000 connections, and peak RSS after sustained load |
| `pressure scaling` | Requests/s, p99, and errors with 100, 1k, 5k, and 10k closed-loop clients |
| `components parser` | `HttpRequest::new` against `httparse` |
| `components router_*` | `Server::invoke` against `matchit`, by route count and by path depth |

`wire_post_4kb` overlaps with `cargo bench --bench compare` on the development branch and uses the same workload. It's here so that all of these results come from one harness. That bench runs each server inside the bench process with no pinning, while this crate gives each server its own process and pinned CPUs, so numbers from the two aren't interchangeable.

## Reading the results

**Connection handling dominates.** Builder closes the connection after every response, so every wire and pressure test opens a new connection per request for every server. Connection setup and teardown take up most of the time, which hides differences in parsing and routing. These results don't predict how Builder would compare in a keep-alive deployment, and the frameworks aren't being run the way they usually are. The keep-alive reference group shows how much that matters to them.

**This measures whatever Builder code is checked out.** The path dependency means the benches measure this checkout's Builder, which is currently blocking with one thread per connection, not the epoll work on `tcp-listener-optimization`. The benches only use Builder's public API, so they're expected to work unchanged after that merges, but that hasn't been tested.

**The workloads aren't identical on the wire.**
- Builder omits the `Date` and `Server` headers that some others send, so it does slightly less work per response.
- The std server is a floor, not a framework. It does no real parsing or routing, so it shows how much of the time is the kernel.
- All servers use a listen backlog of 4096. `std` and `tiny_http` default to 128, so they set it through `std_listener` in `src/lib.rs`.
- Worker counts are each framework's default, which follows the 8 CPUs they're pinned to, so axum runs 9 threads and actix 10. Builder and std start a new thread per connection. tiny_http also needs a thread per open connection (1009 threads while holding 1000), but it reuses them from a pool.

**Comparing the components.**
- `httparse` only tokenizes, while `HttpRequest::new` also validates and builds its parsed form. httparse is a floor rather than an equal.
- Builder's route lookup isn't public, so its side goes through `Server::invoke`. That includes its version, method, and `transfer-encoding` checks and the `response.status_code()` call for its log line, which runs even with logging off. The matchit side only adds a method-array lookup and a handler call, so some of the gap is that extra work rather than the lookup.
- The routes are static, because Builder has no path parameters. Parameter matching, where matchit does most of its work, isn't tested.

**Memory undercounts threads.** RSS excludes kernel thread stacks and socket buffers, which makes thread-per-connection servers look cheaper than they are. Read the thread counts beside it. The held connections send an unfinished request, and are sampled about 2s after connecting, inside Builder's 5s idle timeout. The "Still open" column shows if a server closed any early.

**Latency is measured from the scheduled send time.** The open-loop client sends at a fixed rate whatever the server is doing, so a stall shows up in the numbers of every request behind it. A request that takes over 10s counts as an error. Past 10,000 unfinished requests the client skips sends and counts each as an error.

**One stall can dominate a tail number.** Each load level runs once, and test runs on this WSL2 laptop showed stalls of 20-160ms on every server tested, at levels that don't line up with load. The same server at the same rate gave a p99 of 135ms on one level and 0.8ms on another. The client's send loop stayed on time through them (the "Client lag" column), but the cause wasn't found, so the machine, the kernel, or the client's worker threads are all still possible. Trust a p99 only if it's consistent across repeated runs and rises with load. p50 is much more stable.

**10k clients doesn't mean 10k open connections.** `pressure scaling` sends short requests in a closed loop, so most clients at the higher levels are waiting to connect rather than holding a connection the server is working on. That's why Builder's thread count stays well below the client count, and why this test doesn't reach thread limits. At those levels the accept queue overflows: the kernel's `ListenOverflows` counter rose by about 47,000 during one short Builder run. The multi-second p99s fit clients retrying their dropped connection attempts, though that link wasn't measured directly.

**The machine limits how far to trust these.** Server and client share one machine, split across logical CPUs 0-7 and 8-15. On the i7-1260P these were measured within 6% of each other under load, but under WSL2 the pinning applies to virtual CPUs, which the host can move between its performance and efficiency cores. Thermal throttling can also favor whichever server runs first. Throughput at the same setting varied by about 20% between test runs, so only compare numbers from the same run, and don't read much into differences smaller than that.
