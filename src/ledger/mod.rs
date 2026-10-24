//! The ledger core: the only code that writes to SQLite (AGENTS.md rule 6).
//! Telegram handlers, the ingestor and the scheduler call into it.

mod cards;

pub use cards::{Card, NewCard};

use std::sync::{Mutex, MutexGuard};

use diesel::SqliteConnection;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LedgerError {
    /// Input the owner can fix; the message says how.
    #[error("{0}")]
    InvalidCard(String),
    #[error(
        "{0} already gets emails from that sender with the same card digits, \
         so an email could not tell the two cards apart"
    )]
    CardConflict(String),
    #[error("{0} is already registered")]
    DuplicateCard(String),
    #[error("card {card_id} has unreadable notify_senders: {source}")]
    CorruptSenders {
        card_id: i32,
        source: serde_json::Error,
    },
    #[error("SQLite error: {0}")]
    Db(#[from] diesel::result::Error),
}

/// Owns the one database connection.
///
/// The connection sits behind a `Mutex` so `&self` methods can be called from
/// many async handlers at once (via `Arc<Ledger>`), one at a time. Diesel is
/// synchronous, so a call blocks its tokio thread briefly; for a one-user bot
/// with millisecond queries that is fine.
pub struct Ledger {
    conn: Mutex<SqliteConnection>,
}

impl Ledger {
    /// Takes a connection that `db::open` has already migrated.
    pub fn new(conn: SqliteConnection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Locks the connection. A poisoned lock means an earlier ledger call
    /// panicked mid-write; panicking again keeps that failure loud instead of
    /// carrying on with a connection in an unknown state.
    fn conn(&self) -> MutexGuard<'_, SqliteConnection> {
        self.conn.lock().expect("ledger connection lock poisoned")
    }
}

#[cfg(test)]
pub(crate) fn test_ledger() -> Ledger {
    let conn = crate::db::open(std::path::Path::new(":memory:")).unwrap();
    Ledger::new(conn)
}
