// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg(feature = "sqlite")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use builder::database::{Database, Error, FromRow, Result, Row, Value};
use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Status};
use builder::ip_address::IpAddress;
use builder::server::Server;

const MIGRATIONS: &[&str] = &[
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
    "ALTER TABLE users ADD COLUMN age INTEGER",
];

// A database file of its own for each test, removed when the test ends
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("builder-test-{}-{}.db", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let db = Self { path: std::env::temp_dir().join(name) };
        db.remove();
        db
    }

    fn open(&self) -> Database {
        let db = Database::open(&self.path).unwrap();
        db.migrate(MIGRATIONS).unwrap();
        db
    }

    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.path.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

#[derive(Debug, PartialEq)]
struct User {
    id: i64,
    name: String,
    age: Option<i64>,
}

impl FromRow for User {
    fn from_row(row: &Row) -> Result<Self> {
        Ok(User { id: row.get(0)?, name: row.get("name")?, age: row.get(2)? })
    }
}

#[test]
fn execute_and_query() {
    let file = TempDb::new();
    let db = file.open();

    assert_eq!(db.execute("INSERT INTO users (name, age) VALUES (?, ?)", ("ann", 30)).unwrap(), 1);
    assert_eq!(db.execute("INSERT INTO users (name) VALUES (?)", ("bob",)).unwrap(), 1);

    // Tuples
    let rows: Vec<(i64, String)> = db.query("SELECT id, name FROM users ORDER BY id", ()).unwrap();
    assert_eq!(rows, vec![(1, "ann".to_owned()), (2, "bob".to_owned())]);

    // Structs
    let users: Vec<User> = db.query("SELECT id, name, age FROM users WHERE age > ?", (18,)).unwrap();
    assert_eq!(users, vec![User { id: 1, name: "ann".to_owned(), age: Some(30) }]);

    // Single columns
    let count: Option<i64> = db.query_one("SELECT count(*) FROM users", ()).unwrap();
    assert_eq!(count, Some(2));
    let names: Vec<String> = db.query("SELECT name FROM users ORDER BY name DESC", ()).unwrap();
    assert_eq!(names, ["bob", "ann"]);

    // No rows is None, and a NULL column is Some(None)
    assert_eq!(db.query_one::<User>("SELECT id, name, age FROM users WHERE id = ?", (99,)).unwrap(), None);
    assert_eq!(db.query_one::<Option<i64>>("SELECT age FROM users WHERE name = ?", ("bob",)).unwrap(), Some(None));
}

#[test]
fn values_round_trip() {
    let file = TempDb::new();
    let db = file.open();
    db.execute("CREATE TABLE things (i INTEGER, r REAL, t TEXT, b BLOB, flag INTEGER, missing TEXT)", ()).unwrap();

    let name = String::from("text");
    db.execute("INSERT INTO things VALUES (?, ?, ?, ?, ?, ?)", (7i64, 1.5, &name, vec![1u8, 2, 3], true, None::<String>))
        .unwrap();

    let row: (i64, f64, String, Vec<u8>, bool, Option<String>) = db.query_one("SELECT * FROM things", ()).unwrap().unwrap();
    assert_eq!(row, (7, 1.5, name, vec![1, 2, 3], true, None));

    // Untyped, for a column whose type is not known ahead of time
    let raw: Vec<Value> = db.query("SELECT t FROM things", ()).unwrap();
    assert_eq!(raw, [Value::Text("text".to_owned())]);

    // Arrays, and a list built at runtime
    assert_eq!(db.query_one::<i64>("SELECT count(*) FROM things WHERE i IN (?, ?)", [7, 8]).unwrap(), Some(1));
    let params: Vec<Value> = vec![7.into(), "text".into()];
    assert_eq!(db.query_one::<i64>("SELECT count(*) FROM things WHERE i = ? AND t = ?", params).unwrap(), Some(1));

    // Reading a column as the wrong type is an error that says so, not a panic or a silent conversion
    let wrong = db.query_one::<i64>("SELECT t FROM things", ()).unwrap_err();
    assert!(wrong.to_string().contains("text"), "{wrong}");
    assert!(db.query_one::<String>("SELECT missing FROM things", ()).is_err());
    assert!(db.query_one::<(i64, i64)>("SELECT i FROM things", ()).is_err());
}

#[test]
fn bad_sql_is_an_error() {
    let file = TempDb::new();
    let db = file.open();
    assert!(db.execute("INSERT INTO nowhere VALUES (1)", ()).is_err());
    // NOT NULL is enforced
    assert!(db.execute("INSERT INTO users (name) VALUES (?)", (None::<String>,)).is_err());
    // The database still works afterwards
    assert_eq!(db.query_one::<i64>("SELECT count(*) FROM users", ()).unwrap(), Some(0));
}

#[test]
fn migrations_run_once() {
    let file = TempDb::new();
    {
        let db = file.open();
        db.execute("INSERT INTO users (name, age) VALUES (?, ?)", ("ann", 30)).unwrap();
        // Running them again on the open database does nothing (CREATE TABLE would fail if it ran twice)
        db.migrate(MIGRATIONS).unwrap();
    }

    // Reopening keeps the data and only runs the migration that was added
    let db = Database::open(&file.path).unwrap();
    let mut more = MIGRATIONS.to_vec();
    more.push("CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users (id))");
    db.migrate(&more).unwrap();
    assert_eq!(db.query_one::<i64>("SELECT count(*) FROM users", ()).unwrap(), Some(1));
    assert_eq!(db.execute("INSERT INTO posts (user_id) VALUES (?)", (1,)).unwrap(), 1);
    // Foreign keys are enforced
    assert!(db.execute("INSERT INTO posts (user_id) VALUES (?)", (99,)).is_err());
}

#[test]
fn failed_migration_changes_nothing() {
    let file = TempDb::new();
    let db = Database::open(&file.path).unwrap();
    let broken = ["CREATE TABLE a (id INTEGER)", "CREATE TABLE b (id INTEGER); THIS IS NOT SQL"];
    assert!(db.migrate(&broken).is_err());
    // The first migration stays and the second left nothing behind, so a fixed list picks up from there
    let fixed = [broken[0], "CREATE TABLE b (id INTEGER)"];
    db.migrate(&fixed).unwrap();
    assert_eq!(db.query_one::<i64>("SELECT count(*) FROM b", ()).unwrap(), Some(0));
}

#[test]
fn transaction_commits_or_rolls_back() {
    let file = TempDb::new();
    let db = file.open();
    let count = || db.query_one::<i64>("SELECT count(*) FROM users", ()).unwrap().unwrap();

    let seen = db
        .transaction(|tx| {
            tx.execute("INSERT INTO users (name) VALUES (?)", ("ann",))?;
            tx.execute("INSERT INTO users (name) VALUES (?)", ("bob",))?;
            tx.query::<String>("SELECT name FROM users ORDER BY id", ())
        })
        .unwrap();
    assert_eq!(seen, ["ann", "bob"]);
    assert_eq!(count(), 2);

    // An error undoes what came before it
    let failed: Result<()> = db.transaction(|tx| {
        tx.execute("INSERT INTO users (name) VALUES (?)", ("cat",))?;
        tx.execute("INSERT INTO nowhere VALUES (1)", ())?;
        Ok(())
    });
    assert!(failed.is_err());
    assert_eq!(count(), 2);

    // So does an error of the caller's own
    let failed: Result<()> = db.transaction(|tx| {
        tx.execute("DELETE FROM users", ())?;
        Err(Error::new("changed my mind"))
    });
    assert!(failed.is_err());
    assert_eq!(count(), 2);

    // And a panic
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<()> = db.transaction(|tx| {
            tx.execute("DELETE FROM users", ())?;
            panic!("handler bug");
        });
    }));
    assert!(panicked.is_err());
    assert_eq!(count(), 2);
}

#[test]
fn threads_share_a_database() {
    const THREADS: i64 = 8;
    const PER_THREAD: i64 = 50;

    let file = TempDb::new();
    let db = file.open();
    db.execute("INSERT INTO users (name, age) VALUES ('counter', 0)", ()).unwrap();

    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let db = &db;
            scope.spawn(move || {
                for i in 0..PER_THREAD {
                    db.execute("INSERT INTO users (name) VALUES (?)", (format!("{t}-{i}"),)).unwrap();
                    // A read followed by a write that depends on it, which loses updates unless it is one transaction
                    db.transaction(|tx| {
                        let age: i64 = tx.query_one("SELECT age FROM users WHERE id = 1", ())?.unwrap();
                        tx.execute("UPDATE users SET age = ? WHERE id = 1", (age + 1,))
                    })
                    .unwrap();
                    db.query::<(i64, String)>("SELECT id, name FROM users LIMIT 5", ()).unwrap();
                }
            });
        }
    });

    let total = THREADS * PER_THREAD;
    assert_eq!(db.query_one::<i64>("SELECT count(*) FROM users", ()).unwrap(), Some(total + 1));
    assert_eq!(db.query_one::<i64>("SELECT age FROM users WHERE id = 1", ()).unwrap(), Some(total));
}

struct App {
    db: Database,
}

#[test]
fn handler_reads_the_database() {
    let file = TempDb::new();
    let db = file.open();
    db.execute("INSERT INTO users (name) VALUES (?), (?)", ("ann", "bob")).unwrap();

    let mut server = Server::new(App { db });
    server.add_endpoint(Method::Get, "/users", |_request: &HttpRequest, state: &App| -> HttpResponse {
        match state.db.query::<String>("SELECT name FROM users ORDER BY id", ()) {
            Ok(names) => HttpResponseBuilder::from_text(Status::Ok, names.join(",")).build(),
            Err(_) => HttpResponseBuilder::new(Status::InternalServerError).build(),
        }
    });

    let request = HttpRequest::new("GET /users HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
    let response = server.invoke(&request, &IpAddress::from([127, 0, 0, 1]));
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.body(), b"ann,bob");
}
