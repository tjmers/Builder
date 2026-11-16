#!/bin/bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# This will create a new builder project in the specified directory
# The project serves a hello world page whose text is read from a SQLite database

# $1 will be the directory name

set -euo pipefail

if [ $# -ne 1 ]; then
  echo "Usage: $0 <directory>" >&2
  exit 1
fi

# The builder checkout this script lives in, which the new project depends on by path
builder_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Create any directories necessary
mkdir -p "$1/migrations"
cd "$1"

# Run cargo init
cargo init --bin

# Add builder as a dependency to Cargo.toml
cargo add builder --path "$builder_dir" --features sqlite

# The database files are created next to the project when it runs
printf '*.db\n*.db-wal\n*.db-shm\n' >> .gitignore

# The first migration, which creates the schema
# Later changes to the schema go in new files, added to the end of the list in main.rs
cat > migrations/001_init.sql <<'EOF'
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    body TEXT NOT NULL
);

INSERT INTO messages (body) VALUES ('Hello, World');
EOF

# Add the main file with the get endpoint for index that returns a simple hello world
cat > src/main.rs <<'EOF'
use builder::database::{self, Database};
use builder::http_request::{HttpRequest, Method};
use builder::http_response::{HttpResponse, HttpResponseBuilder, Reply, Status};
use builder::logger::LOGGER;
use builder::server::Server;
use builder::text::{TextFile, TextType};

const PORT: i32 = 8080;

// What every request handler is given
struct App {
    db: Database,
}

fn index(_request: &HttpRequest, state: &App) -> HttpResponse<'static> {
    match state.db.query_one::<String>("SELECT body FROM messages WHERE id = ?", (1,)) {
        Ok(Some(body)) => {
            let page = format!("<h1>{body}</h1>");
            HttpResponseBuilder::from(Reply::new(Status::Ok, TextFile::new(page, TextType::Html))).build()
        }
        Ok(None) => HttpResponseBuilder::new(Status::NotFound).build(),
        Err(e) => {
            builder::log_error!("GET / failed: {e}");
            HttpResponseBuilder::new(Status::InternalServerError).build()
        }
    }
}

fn main() -> database::Result<()> {
    LOGGER.print_to_stdout();

    let db = Database::open("app.db")?;
    // Every change to the schema, oldest first: only ever add to the end
    db.migrate(&[include_str!("../migrations/001_init.sql")])?;

    let mut server = Server::new(App { db });
    server.add_endpoint(Method::Get, "/", index);

    println!("Listening on http://localhost:{PORT}");
    server.run(PORT);
    Ok(())
}
EOF
