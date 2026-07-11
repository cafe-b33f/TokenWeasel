//! Shared SQLite core for the usage and identity stores.
//!
//! [`Database`] opens the database file and applies the declarative schema
//! at compile time. It exposes an [`r2d2`] connection pool via [`Database::pool`],
//! and [`DbError`] is the common error type.

use std::time::{SystemTime, UNIX_EPOCH};

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

/// Type alias for the r2d2 connection pool used by all stores.
pub type DbPool = Pool<SqliteConnectionManager>;

/// Database-level error type that abstracts over raw SQLite errors.
///
/// This type is the single error type returned by every public database
/// method. It replaces leaking `rusqlite::Error` into the auth and server
/// crates.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A UNIQUE constraint was violated (e.g. duplicate username).
    #[error("unique constraint violation")]
    UniqueViolation,
    /// System clock is before Unix epoch.
    #[error("system clock before Unix epoch")]
    Clock,
    /// Connection pool error.
    #[error("connection pool error: {0}")]
    Pool(String),
    /// Any other SQLite error.
    #[error(transparent)]
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        match e.sqlite_error() {
            Some(se) if se.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => {
                DbError::UniqueViolation
            }
            _ => DbError::Sqlite(e),
        }
    }
}

impl From<r2d2::Error> for DbError {
    fn from(e: r2d2::Error) -> Self {
        DbError::Pool(e.to_string())
    }
}

/// Open a new SQLite connection at `path` and apply the standard configuration
/// (WAL journal, NORMAL synchronous, 5 s busy timeout) shared across all
/// database connections.
///
/// This function is the single point where SQLite connection settings are
/// configured. Every `Connection` - writer, read, and energy - goes through it.
pub fn open_connection(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// Current wall-clock time as whole seconds since the Unix epoch, or `None` if
/// the system clock is before 1970.
pub fn now_unix_secs() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Database - shared SQLite file & schema
// ---------------------------------------------------------------------------

/// Shared state for the SQLite database file. Opens the file, runs schema
/// migrations, and provides [`Database::connect`] so domain stores can open
/// their own connections.
///
/// A single `Database` should be created at startup and handed to each store.
#[derive(Debug)]
pub struct Database {
    path: String,
    pool: DbPool,
}

impl Database {
    /// Open the SQLite database at `path`, enable WAL mode, apply the
    /// declarative schema, and return the database handle.
    ///
    /// Returns [`DbError`] if any step fails.
    pub fn open(path: &str) -> Result<Self, DbError> {
        // Validate path immediately - fails fast instead of waiting 30 s
        // for the pool's retry loop on a connection that can never succeed.
        let conn = open_connection(path)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        // conn dropped here

        let manager = SqliteConnectionManager::file(path).with_init(|conn: &mut Connection| {
            conn.pragma_update(None, "foreign_keys", "ON").map(|_| ())?;
            conn.pragma_update(None, "journal_mode", "WAL")
                .map(|_| ())?;
            conn.pragma_update(None, "synchronous", "NORMAL")
                .map(|_| ())?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map(|_| ())?;
            Ok(())
        });
        let pool = Pool::builder().build_unchecked(manager);

        Ok(Database {
            path: path.to_string(),
            pool,
        })
    }

    /// Open a fresh configured connection to the database file.
    pub fn connect(&self) -> Result<Connection, DbError> {
        Ok(open_connection(&self.path)?)
    }

    /// Return a clone of the connection pool.
    ///
    /// [`r2d2::Pool`] is cheaply cloneable - cloning only increments an
    /// atomic reference count.
    pub fn pool(&self) -> DbPool {
        self.pool.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
