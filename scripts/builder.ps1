# This will create a new builder project in the specified directory
# The project serves a hello world page whose text is read from a SQLite database
# PowerShell version of builder.sh; works on Windows PowerShell 5.1 and PowerShell 7+

# The first argument will be the directory name
param(
    [Parameter(Position = 0)]
    [string]$Directory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Extra arguments are rejected by PowerShell itself, so only a missing one needs checking
if (-not $Directory) {
    [Console]::Error.WriteLine("Usage: $($MyInvocation.MyCommand.Name) <directory>")
    exit 1
}

# Native commands don't stop the script on failure, so check their exit codes
function Invoke-Native {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

# Writes UTF-8 without a BOM (Windows PowerShell's -Encoding utf8 adds one, which would end up in the SQL)
$utf8 = New-Object System.Text.UTF8Encoding $false

# The builder checkout this script lives in, which the new project depends on by path
$builderDir = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

# Create any directories necessary
New-Item -ItemType Directory -Force -Path (Join-Path $Directory 'migrations') | Out-Null
$projectDir = (Resolve-Path $Directory).Path

# Change directory only for the duration of the script, so the caller's location is left alone
Push-Location $projectDir
try {
    # Run cargo init
    Invoke-Native cargo @('init', '--bin')

    # Add builder as a dependency to Cargo.toml
    Invoke-Native cargo @('add', 'builder', '--path', $builderDir, '--features', 'sqlite')

    # The database files are created next to the project when it runs
    [System.IO.File]::AppendAllText((Join-Path $projectDir '.gitignore'), "*.db`n*.db-wal`n*.db-shm`n", $utf8)

    # The first migration, which creates the schema
    # Later changes to the schema go in new files, added to the end of the list in main.rs
    $migration = @'
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    body TEXT NOT NULL
);

INSERT INTO messages (body) VALUES ('Hello, World');

'@
    [System.IO.File]::WriteAllText((Join-Path $projectDir 'migrations\001_init.sql'), $migration, $utf8)

    # Add the main file with the get endpoint for index that returns a simple hello world
    $main = @'
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

'@
    [System.IO.File]::WriteAllText((Join-Path $projectDir 'src\main.rs'), $main, $utf8)
}
finally {
    Pop-Location
}
