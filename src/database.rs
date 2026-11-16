// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// SQLite access for request handlers
// A Database goes in the server's state, and every handler thread can query it at once:
//
//     struct App { db: Database }
//     let users: Vec<(i64, String)> = state.db.query("SELECT id, name FROM users WHERE id > ?", (5,))?;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, TransactionBehavior};

// Everything public here is builder's own, so code written against it does not depend on which database is behind it
// rusqlite stays inside this file

// How long a statement waits for another connection's write to finish before failing with a busy error
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// Why a database call failed
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    // An error of the caller's own, e.g. to abandon a transaction
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// Not a From impl, which would put rusqlite's error type in the public API
fn backend(error: rusqlite::Error) -> Error {
    Error::new(error.to_string())
}

// A value going into a statement as a parameter, or coming out of a column
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "an integer",
            Value::Real(_) => "a real",
            Value::Text(_) => "text",
            Value::Blob(_) => "a blob",
        }
    }
}

macro_rules! value_from {
    ($($t:ty => |$v:ident| $e:expr),* $(,)?) => {$(
        impl From<$t> for Value {
            fn from($v: $t) -> Self {
                $e
            }
        }
    )*};
}

value_from! {
    i64 => |v| Value::Integer(v),
    i32 => |v| Value::Integer(v.into()),
    u32 => |v| Value::Integer(v.into()),
    bool => |v| Value::Integer(v.into()),
    f64 => |v| Value::Real(v),
    String => |v| Value::Text(v),
    &str => |v| Value::Text(v.to_owned()),
    &String => |v| Value::Text(v.clone()),
    Vec<u8> => |v| Value::Blob(v),
    &[u8] => |v| Value::Blob(v.to_vec()),
}

// None is NULL
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Value::Null, Into::into)
    }
}

// A type a column can be read as
pub trait FromValue: Sized {
    fn from_value(value: Value) -> Result<Self>;
}

fn mismatch<T>(value: &Value) -> Error {
    Error::new(format!("Cannot read {} as {}", value.type_name(), std::any::type_name::<T>()))
}

impl FromValue for Value {
    fn from_value(value: Value) -> Result<Self> {
        Ok(value)
    }
}

impl FromValue for i64 {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Integer(v) => Ok(v),
            other => Err(mismatch::<Self>(&other)),
        }
    }
}

impl FromValue for i32 {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Integer(v) => v.try_into().map_err(|_| Error::new(format!("{v} does not fit in an i32"))),
            other => Err(mismatch::<Self>(&other)),
        }
    }
}

impl FromValue for bool {
    fn from_value(value: Value) -> Result<Self> {
        i64::from_value(value).map(|v| v != 0)
    }
}

impl FromValue for f64 {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Real(v) => Ok(v),
            // What sum() of an integer column gives, for one
            Value::Integer(v) => Ok(v as f64),
            other => Err(mismatch::<Self>(&other)),
        }
    }
}

impl FromValue for String {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Text(v) => Ok(v),
            other => Err(mismatch::<Self>(&other)),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Blob(v) => Ok(v),
            other => Err(mismatch::<Self>(&other)),
        }
    }
}

// For a column that may be NULL
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

// One row of a query's results
pub struct Row<'a>(&'a rusqlite::Row<'a>);

impl Row<'_> {
    // Reads a column by its position, counting from 0, or by its name
    pub fn get<T: FromValue>(&self, column: impl Column) -> Result<T> {
        T::from_value(column.read(self)?)
    }
}

// What says which column of a Row to read: its position or its name
pub trait Column {
    #[doc(hidden)]
    fn read(self, row: &Row<'_>) -> Result<Value>;
}

fn to_value(value: ValueRef<'_>) -> Result<Value> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => Value::Integer(v),
        ValueRef::Real(v) => Value::Real(v),
        ValueRef::Text(v) => {
            Value::Text(std::str::from_utf8(v).map_err(|_| Error::new("A text column is not UTF-8"))?.to_owned())
        }
        ValueRef::Blob(v) => Value::Blob(v.to_vec()),
    })
}

impl Column for usize {
    fn read(self, row: &Row<'_>) -> Result<Value> {
        to_value(row.0.get_ref(self).map_err(backend)?)
    }
}

impl Column for &str {
    fn read(self, row: &Row<'_>) -> Result<Value> {
        to_value(row.0.get_ref(self).map_err(backend)?)
    }
}

// The parameters of a statement, one for each ? in it, in order: (), a tuple of up to 12 values of any types, an
// array of values of one type, or a Vec<Value> for a list built at runtime
pub trait Params {
    fn into_values(self) -> Vec<Value>;
}

impl Params for () {
    fn into_values(self) -> Vec<Value> {
        Vec::new()
    }
}

impl Params for Vec<Value> {
    fn into_values(self) -> Vec<Value> {
        self
    }
}

impl<T: Into<Value>, const N: usize> Params for [T; N] {
    fn into_values(self) -> Vec<Value> {
        self.into_iter().map(Into::into).collect()
    }
}

// A type that can be read from one row of a query's results
// Implemented for the common column types on their own (for single column queries) and for tuples of up to 12
// columns, and by hand for a struct:
//
//     impl FromRow for User {
//         fn from_row(row: &Row) -> Result<Self> {
//             Ok(User { id: row.get(0)?, name: row.get("name")? })
//         }
//     }
pub trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> Result<Self>;
}

macro_rules! from_row_scalar {
    ($($t:ty),*) => {$(
        impl FromRow for $t {
            fn from_row(row: &Row<'_>) -> Result<Self> {
                row.get(0)
            }
        }

        // For a column that may be NULL
        impl FromRow for Option<$t> {
            fn from_row(row: &Row<'_>) -> Result<Self> {
                row.get(0)
            }
        }
    )*};
}

from_row_scalar!(i64, i32, f64, bool, String, Vec<u8>, Value);

// Tuples as rows and as parameters
macro_rules! tuples {
    ($($t:ident $i:tt),*) => {
        impl<$($t: FromValue),*> FromRow for ($($t,)*) {
            fn from_row(row: &Row<'_>) -> Result<Self> {
                Ok(($(row.get($i)?,)*))
            }
        }

        impl<$($t: Into<Value>),*> Params for ($($t,)*) {
            fn into_values(self) -> Vec<Value> {
                vec![$(self.$i.into()),*]
            }
        }
    };
}

tuples!(A 0);
tuples!(A 0, B 1);
tuples!(A 0, B 1, C 2);
tuples!(A 0, B 1, C 2, D 3);
tuples!(A 0, B 1, C 2, D 3, E 4);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
tuples!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);

fn bind(params: impl Params) -> impl rusqlite::Params {
    rusqlite::params_from_iter(params.into_values().into_iter().map(|value| match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Integer(v) => rusqlite::types::Value::Integer(v),
        Value::Real(v) => rusqlite::types::Value::Real(v),
        Value::Text(v) => rusqlite::types::Value::Text(v),
        Value::Blob(v) => rusqlite::types::Value::Blob(v),
    }))
}

// The statements are prepared once per connection and kept, so running the same SQL again does not parse it again

fn execute(conn: &Connection, sql: &str, params: impl Params) -> Result<usize> {
    conn.prepare_cached(sql).and_then(|mut statement| statement.execute(bind(params))).map_err(backend)
}

fn query<T: FromRow>(conn: &Connection, sql: &str, params: impl Params) -> Result<Vec<T>> {
    let mut statement = conn.prepare_cached(sql).map_err(backend)?;
    let mut rows = statement.query(bind(params)).map_err(backend)?;
    let mut found = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        found.push(T::from_row(&Row(row))?);
    }
    Ok(found)
}

fn query_one<T: FromRow>(conn: &Connection, sql: &str, params: impl Params) -> Result<Option<T>> {
    let mut statement = conn.prepare_cached(sql).map_err(backend)?;
    let mut rows = statement.query(bind(params)).map_err(backend)?;
    rows.next().map_err(backend)?.map(|row| T::from_row(&Row(row))).transpose()
}

// A SQLite database file
// It is Sync, so it can live in the state of a Server and be used by every handler at once
pub struct Database {
    path: PathBuf,
    // The connections nobody is using
    // A connection serves one thread at a time, so each call takes one from here (or opens another) and puts it
    // back afterwards, which leaves at most as many connections as there have been calls running at once
    idle: Mutex<Vec<Connection>>,
}

// A connection taken from a Database, which goes back to it when this is dropped
struct Pooled<'a> {
    database: &'a Database,
    conn: Option<Connection>,
}

impl Pooled<'_> {
    fn get(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("the connection is only taken on drop")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // A handler that panicked while holding the lock has not left the list in a bad state
            self.database.idle.lock().unwrap_or_else(PoisonError::into_inner).push(conn);
        }
    }
}

impl Database {
    // Opens the database file at path, creating it if it does not exist
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database = Self {
            path: path.as_ref().to_owned(),
            idle: Mutex::new(Vec::new()),
        };
        // Open one connection now, so a path that cannot be opened fails here and not in the first handler
        drop(database.connection()?);
        Ok(database)
    }

    fn connection(&self) -> Result<Pooled<'_>> {
        let idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let conn = match idle {
            Some(conn) => conn,
            None => {
                let conn = Connection::open(&self.path).map_err(backend)?;
                conn.busy_timeout(BUSY_TIMEOUT).map_err(backend)?;
                // WAL lets handlers read while another one writes
                // Foreign keys are not enforced unless each connection asks for it
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;").map_err(backend)?;
                conn
            }
        };
        Ok(Pooled { database: self, conn: Some(conn) })
    }

    // Runs a statement that returns no rows (INSERT, UPDATE, DELETE, CREATE, ...) and returns how many rows it changed
    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        execute(self.connection()?.get(), sql, params)
    }

    // Runs a query and returns all of its rows
    pub fn query<T: FromRow>(&self, sql: &str, params: impl Params) -> Result<Vec<T>> {
        query(self.connection()?.get(), sql, params)
    }

    // Runs a query and returns its first row, or None if it has no rows
    pub fn query_one<T: FromRow>(&self, sql: &str, params: impl Params) -> Result<Option<T>> {
        query_one(self.connection()?.get(), sql, params)
    }

    // Runs body with everything it does through the Transaction applied together or not at all
    // The changes are kept if body returns Ok, and undone if it returns Err or panics
    pub fn transaction<T>(&self, body: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.connection()?;
        // Immediate takes the write lock up front, where the busy timeout can wait for it
        // Taking it at the first write instead fails straight away if another writer got in after this one's reads
        let transaction = Transaction(conn.get().transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?);
        // Dropping the transaction without committing rolls it back
        let value = body(&transaction)?;
        transaction.0.commit().map_err(backend)?;
        Ok(value)
    }

    // Brings the schema up to date: migrations is every change ever made to it, oldest first, and the ones this
    // database has not had yet are run in order
    // Only ever add to the end of the list, since the database remembers how many it has run and not what they were
    // Each entry may hold several statements separated by semicolons
    pub fn migrate(&self, migrations: &[&str]) -> Result<()> {
        let mut conn = self.connection()?;
        loop {
            // The count is read inside the transaction that runs the migration, so two processes starting at once
            // cannot both run the same one
            let transaction = conn.get().transaction_with_behavior(TransactionBehavior::Immediate).map_err(backend)?;
            let applied: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(backend)?;
            let Some(sql) = usize::try_from(applied).ok().and_then(|applied| migrations.get(applied)) else {
                return Ok(());
            };
            transaction.execute_batch(sql).map_err(backend)?;
            transaction.pragma_update(None, "user_version", applied + 1).map_err(backend)?;
            transaction.commit().map_err(backend)?;
            log_info!("Applied database migration {}", applied + 1);
        }
    }
}

// The database inside a call to Database::transaction
pub struct Transaction<'a>(rusqlite::Transaction<'a>);

impl Transaction<'_> {
    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        execute(&self.0, sql, params)
    }

    pub fn query<T: FromRow>(&self, sql: &str, params: impl Params) -> Result<Vec<T>> {
        query(&self.0, sql, params)
    }

    pub fn query_one<T: FromRow>(&self, sql: &str, params: impl Params) -> Result<Option<T>> {
        query_one(&self.0, sql, params)
    }
}
