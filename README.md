# Builder

Builder is an early-stage Rust project intended to become a full-stack web framework. It aims to combine efficient runtime behavior with clear implementations that make application development straightforward.

## Project direction

The goal is to bring server-side application logic and HTML rendering into one framework. The planned approach includes HTML template files that are compiled into Rust source files at build time.

The template syntax, generated code, application APIs, and overall design are still being explored. This describes the intended direction, rather than a finalized design or a set of features available today.

Development currently focuses on the underlying HTTP server and platform-specific socket implementations described below.

## Supported Architectures

Builder currently requires a 64-bit Linux or Windows environment.

## Current implementation

The sockets are specifically implemented via os-specific sockets because they will later be optimized for speed and the os-control is desired.

The shared request framing lives in `src/tcp_listener.rs`, and each platform supplies its own socket calls: `src/tcp_listener/linux.rs` uses libc and epoll, and `src/tcp_listener/windows.rs` uses Winsock through windows-sys, with a completion port and `AcceptEx`.

## Database

The `sqlite` feature adds `builder::database`, a small SQL-first layer over SQLite (bundled through rusqlite, so there is nothing to install). It is off by default.

```toml
builder = { path = "...", features = ["sqlite"] }
```

A `Database` goes in the server's state, and every handler can use it at once: each call borrows a connection from a pool that grows as needed.

```rust
use builder::database::{Database, FromRow, Result, Row};

struct App { db: Database }

let db = Database::open("app.db")?;
// Every change to the schema, oldest first. Only the ones this database has not had are run
db.migrate(&[
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
    include_str!("../migrations/002_add_age.sql"),
])?;
let mut server = Server::new(App { db });

// In a handler, with state: &App
state.db.execute("INSERT INTO users (name) VALUES (?)", ("ann",))?;
let users: Vec<(i64, String)> = state.db.query("SELECT id, name FROM users WHERE id > ?", (5,))?;
let count: Option<i64> = state.db.query_one("SELECT count(*) FROM users", ())?;

// Kept if the closure returns Ok, undone if it returns Err or panics
state.db.transaction(|tx| {
    let age: i64 = tx.query_one("SELECT age FROM users WHERE id = ?", (1,))?.unwrap_or(0);
    tx.execute("UPDATE users SET age = ? WHERE id = ?", (age + 1, 1))
})?;
```

Rows are read as a single value, a tuple of up to 12 columns, or any type that implements `FromRow`:

```rust
struct User { id: i64, name: String }

impl FromRow for User {
    fn from_row(row: &Row) -> Result<Self> {
        Ok(User { id: row.get(0)?, name: row.get("name")? })
    }
}

let users: Vec<User> = state.db.query("SELECT id, name FROM users", ())?;
```

Every type in `builder::database` is builder's own (`Row`, `Value`, `Params`, `Error`), so application code does not depend on the SQLite library underneath, and other databases can be added later without changing it.

Handlers return a response rather than a `Result`, so a handler matches on the result and answers with a 500 on an error. `scripts/builder.sh <directory>` creates a new project set up this way, with a database in its state and a first migration. In-memory databases are not supported, since each pooled connection would get a database of its own.

## Benchmarks

`cargo bench --bench throughput` measures end to end throughput (requests/s) over loopback sockets, with 1, 4, 8, 12, and 16 concurrent clients sending a 4kb `POST /` and receiving a 4kb `200` response. Reports are written to `target/criterion/`, and each run is compared against the previous one.

`cargo bench --bench compare` runs the same workload against builder and four reference servers in one criterion group: a minimal thread-per-connection responder on `std::net`, axum (tokio + hyper), actix-web, and tiny_http. Each one serves a prebuilt 4kb body and reads the whole request body first. Every request carries `Connection: close`, since builder closes after each response, so no server gets keep-alive as an advantage. The side by side report is `target/criterion/compare_post_4kb_resp_4kb/report/index.html`. Run a single server with `cargo bench --bench compare -- axum`. A full run takes about 6 minutes.

The client code both benches share is in `benches/common/mod.rs`.

## TODO

- Force crlf internally (src/tcp_listener.rs:38)
