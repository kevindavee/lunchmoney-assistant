//! Opening the SQLite database and running migrations, with Diesel.
//!
//! Only the ledger core writes to the database (AGENTS.md rule 6); this module
//! just hands it a ready, migrated connection.

pub mod schema;

use std::collections::HashSet;
use std::path::Path;

use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use thiserror::Error;

/// Every migration in `migrations/`, copied into the binary at compile time,
/// so the deployed image doesn't need the folder. Never edit a migration
/// that has shipped; add a new one with `diesel migration generate <name>`.
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Errors from diesel_migrations come as a boxed trait object: "some type
/// that implements `Error`", decided at runtime.
type MigrationError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database path is not valid UTF-8")]
    NonUtf8Path,
    #[error("could not open database: {0}")]
    Connect(#[from] diesel::ConnectionError),
    #[error("SQLite error: {0}")]
    Query(#[from] diesel::result::Error),
    #[error("migrations failed: {0}")]
    Migration(MigrationError),
    #[error(
        "database has migration {0}, which this binary doesn't know; \
         refusing to run an older build against a newer database"
    )]
    TooNew(String),
}

/// Opens (creating if needed) the database at `path` and migrates it.
pub fn open(path: &Path) -> Result<SqliteConnection, DbError> {
    let url = path.to_str().ok_or(DbError::NonUtf8Path)?;
    let mut conn = SqliteConnection::establish(url)?;
    configure(&mut conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Per-connection settings. SQLite does not remember these between opens.
fn configure(conn: &mut SqliteConnection) -> Result<(), DbError> {
    // WAL lets the weekly backup read while the bot writes. In-memory test
    // databases ignore it, which is fine.
    conn.batch_execute(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;",
    )?;
    Ok(())
}

/// Applies pending migrations. Diesel records applied ones in its own
/// `__diesel_schema_migrations` table and runs each in a transaction.
fn migrate(conn: &mut SqliteConnection) -> Result<(), DbError> {
    // `MigrationSource<Sqlite>` is a generic trait; naming the backend in
    // angle brackets tells Rust which implementation of `migrations()` we mean.
    let known: HashSet<String> =
        diesel::migration::MigrationSource::<Sqlite>::migrations(&MIGRATIONS)
            .map_err(DbError::Migration)?
            .iter()
            .map(|m| m.name().version().to_string())
            .collect();

    let applied = conn.applied_migrations().map_err(DbError::Migration)?;
    if let Some(unknown) = applied.iter().find(|v| !known.contains(&v.to_string())) {
        return Err(DbError::TooNew(unknown.to_string()));
    }

    for version in conn
        .run_pending_migrations(MIGRATIONS)
        .map_err(DbError::Migration)?
    {
        tracing::info!(%version, "applied migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::schema::{cards, charges, merchant_answers, merchant_docs};
    use super::schema::{messages, payments, pending_inputs, reminders_sent};
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};
    use diesel::result::{DatabaseErrorKind, Error::DatabaseError};

    fn fresh() -> SqliteConnection {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        configure(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    /// Inserts a card and returns its ID. `.returning(...)` uses SQLite's
    /// `RETURNING` clause to get the new row's ID back in the same statement.
    fn insert_card(conn: &mut SqliteConnection) -> i32 {
        diesel::insert_into(cards::table)
            .values((
                cards::bank.eq("BCA"),
                cards::notify_senders.eq(r#"["notif@bca.co.id"]"#),
                cards::created_at.eq(at(24, 0, 0)),
            ))
            .returning(cards::id)
            .get_result(conn)
            .unwrap()
    }

    /// Inserts a card of `bank` with the given card digits (or none).
    fn insert_card_ref(
        conn: &mut SqliteConnection,
        bank: &str,
        card_ref: Option<&str>,
    ) -> QueryResult<usize> {
        diesel::insert_into(cards::table)
            .values((
                cards::bank.eq(bank),
                cards::notify_senders.eq(r#"["noreply.livin@bankmandiri.co.id"]"#),
                cards::card_ref.eq(card_ref),
                cards::created_at.eq(at(24, 0, 0)),
            ))
            .execute(conn)
    }

    fn insert_charge(
        conn: &mut SqliteConnection,
        card_id: i32,
        gmail_msg_id: &str,
    ) -> QueryResult<usize> {
        diesel::insert_into(charges::table)
            .values((
                charges::gmail_msg_id.eq(gmail_msg_id),
                charges::card_id.eq(card_id),
                charges::merchant_raw.eq("SUSHI TEI PIK"),
                charges::amount_idr.eq(1_240_000_i64),
                charges::charged_at.eq(at(16, 13, 14)),
                charges::created_at.eq(at(16, 13, 20)),
            ))
            .execute(conn)
    }

    /// Returns the kind of database error, so tests can check *which*
    /// constraint fired, not just that something failed.
    fn violation(result: QueryResult<usize>) -> DatabaseErrorKind {
        match result {
            Err(DatabaseError(kind, _)) => kind,
            other => panic!("expected a constraint violation, got {other:?}"),
        }
    }

    #[test]
    fn schema_rs_matches_every_migrated_table() {
        // Each query fails if a table in schema.rs is missing from the DB.
        let conn = &mut fresh();
        let counts: Vec<i64> = vec![
            cards::table.count().get_result(conn).unwrap(),
            charges::table.count().get_result(conn).unwrap(),
            payments::table.count().get_result(conn).unwrap(),
            merchant_answers::table.count().get_result(conn).unwrap(),
            reminders_sent::table.count().get_result(conn).unwrap(),
            messages::table.count().get_result(conn).unwrap(),
            pending_inputs::table.count().get_result(conn).unwrap(),
            merchant_docs::table.count().get_result(conn).unwrap(),
        ];
        assert_eq!(counts, vec![0; 8]);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let conn = &mut fresh();
        migrate(conn).unwrap();
        assert_eq!(conn.applied_migrations().unwrap().len(), 1);
    }

    #[test]
    fn refuses_database_from_a_newer_build() {
        let conn = &mut fresh();
        diesel::sql_query(
            "INSERT INTO __diesel_schema_migrations (version) VALUES ('29991231000000')",
        )
        .execute(conn)
        .unwrap();
        assert!(matches!(migrate(conn), Err(DbError::TooNew(v)) if v == "29991231000000"));
    }

    #[test]
    fn charge_round_trips_amount_and_time() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        insert_charge(conn, card, "msg-1").unwrap();
        let (amount, charged_at): (Option<i64>, NaiveDateTime) = charges::table
            .select((charges::amount_idr, charges::charged_at))
            .first(conn)
            .unwrap();
        assert_eq!(amount, Some(1_240_000));
        assert_eq!(charged_at, at(16, 13, 14));
    }

    #[test]
    fn duplicate_gmail_message_is_rejected() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        insert_charge(conn, card, "msg-1").unwrap();
        assert_eq!(
            violation(insert_charge(conn, card, "msg-1")),
            DatabaseErrorKind::UniqueViolation
        );
    }

    #[test]
    fn charge_must_reference_an_existing_card() {
        let conn = &mut fresh();
        assert_eq!(
            violation(insert_charge(conn, 42, "msg-1")),
            DatabaseErrorKind::ForeignKeyViolation
        );
    }

    #[test]
    fn card_ref_is_two_to_four_digits() {
        let conn = &mut fresh();
        for bad in ["1", "12345", "12a4", "****1234", "XX19"] {
            assert_eq!(
                violation(insert_card_ref(conn, "Mandiri", Some(bad))),
                DatabaseErrorKind::CheckViolation,
                "{bad:?} should be rejected"
            );
        }
        insert_card_ref(conn, "Mandiri", Some("7078")).unwrap();
        insert_card_ref(conn, "CIMB Niaga", Some("19")).unwrap();
    }

    #[test]
    fn same_digits_at_same_bank_are_rejected() {
        let conn = &mut fresh();
        insert_card_ref(conn, "Mandiri", Some("1234")).unwrap();
        assert_eq!(
            violation(insert_card_ref(conn, "Mandiri", Some("1234"))),
            DatabaseErrorKind::UniqueViolation
        );
        // Same digits at another bank, or other digits at this bank, are fine.
        insert_card_ref(conn, "Jenius", Some("1234")).unwrap();
        insert_card_ref(conn, "Mandiri", Some("5678")).unwrap();
    }

    #[test]
    fn cards_without_digits_can_share_a_bank() {
        let conn = &mut fresh();
        insert_card_ref(conn, "Mandiri", None).unwrap();
        insert_card_ref(conn, "Mandiri", None).unwrap();
    }

    #[test]
    fn charge_records_how_it_was_read() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        insert_charge(conn, card, "msg-1").unwrap();
        let read_by: String = charges::table
            .select(charges::extracted_by)
            .first(conn)
            .unwrap();
        assert_eq!(read_by, "parser");
        let result = diesel::update(charges::table)
            .set(charges::extracted_by.eq("guess"))
            .execute(conn);
        assert_eq!(violation(result), DatabaseErrorKind::CheckViolation);
    }

    #[test]
    fn payable_cannot_go_negative() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        let result = diesel::update(cards::table.find(card))
            .set(cards::payable_idr.eq(-1_i64))
            .execute(conn);
        assert_eq!(violation(result), DatabaseErrorKind::CheckViolation);
    }

    #[test]
    fn pending_charge_needs_a_rupiah_amount() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        let result = diesel::insert_into(charges::table)
            .values((
                charges::gmail_msg_id.eq("m"),
                charges::card_id.eq(card),
                charges::merchant_raw.eq("X"),
                charges::charged_at.eq(at(16, 0, 0)),
                charges::created_at.eq(at(16, 0, 0)),
            ))
            .execute(conn);
        assert_eq!(violation(result), DatabaseErrorKind::CheckViolation);
    }

    #[test]
    fn foreign_charge_waits_without_rupiah_amount() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        diesel::insert_into(charges::table)
            .values((
                charges::gmail_msg_id.eq("m"),
                charges::card_id.eq(card),
                charges::merchant_raw.eq("STEAM"),
                charges::orig_currency.eq("USD"),
                charges::orig_amount.eq("12.34"),
                charges::status.eq("needs_amount"),
                charges::charged_at.eq(at(16, 0, 0)),
                charges::created_at.eq(at(16, 0, 0)),
            ))
            .execute(conn)
            .unwrap();
    }

    #[test]
    fn unknown_status_is_rejected() {
        let conn = &mut fresh();
        let card = insert_card(conn);
        insert_charge(conn, card, "msg-1").unwrap();
        let result = diesel::update(charges::table)
            .set(charges::status.eq("done"))
            .execute(conn);
        assert_eq!(violation(result), DatabaseErrorKind::CheckViolation);
    }

    #[test]
    fn opens_a_file_database() {
        let dir = std::env::temp_dir().join(format!("expense-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        drop(open(&path).unwrap());
        // Reopening an already-migrated file works and applies nothing new.
        let mut conn = open(&path).unwrap();
        assert_eq!(conn.applied_migrations().unwrap().len(), 1);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
