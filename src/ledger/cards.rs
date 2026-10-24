//! Registered cards: adding them and finding the one an email belongs to.
//!
//! Invariant: an email (its sender plus the card digits it shows) matches at
//! most one card. `register_card` refuses any card that would break it.

use chrono::Utc;
use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error::DatabaseError};

use super::{Ledger, LedgerError};
use crate::db::schema::cards;

/// Longest bank name accepted, in characters.
const MAX_BANK_LEN: usize = 40;

/// A card billing account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub id: i32,
    pub bank: String,
    /// Notification sender addresses, lowercase.
    pub senders: Vec<String>,
    /// Card digits the emails show after the mask, or `None` for banks whose
    /// emails show none.
    pub card_ref: Option<String>,
    pub cutoff_day: Option<i32>,
    pub due_day: Option<i32>,
    pub payable_idr: i64,
}

impl Card {
    /// "Mandiri •7078", or just "Mandiri" when the emails show no digits.
    pub fn label(&self) -> String {
        label(&self.bank, self.card_ref.as_deref())
    }
}

fn label(bank: &str, card_ref: Option<&str>) -> String {
    match card_ref {
        Some(digits) => format!("{bank} •{digits}"),
        None => bank.to_string(),
    }
}

/// A card to register, already checked and normalized. The fields are
/// private, so `NewCard::new` is the only way to make one and an unchecked
/// card cannot reach the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCard {
    bank: String,
    senders: Vec<String>,
    card_ref: Option<String>,
}

impl NewCard {
    pub fn new(bank: &str, senders: &[&str], card_ref: Option<&str>) -> Result<Self, LedgerError> {
        let bank = bank.trim();
        if bank.is_empty() {
            return Err(invalid("the bank name is empty"));
        }
        if bank.chars().count() > MAX_BANK_LEN {
            return Err(invalid(format!(
                "the bank name is longer than {MAX_BANK_LEN} characters"
            )));
        }
        // Usually a sign of "a@x.com, b@y.com" with a space after the comma.
        if bank.contains('@') {
            return Err(invalid(format!(
                "the bank name {bank:?} contains an email address; \
                 separate senders with commas and no spaces"
            )));
        }

        let mut normalized: Vec<String> = Vec::new();
        for raw in senders {
            let sender = normalize_sender(raw)?;
            if !normalized.contains(&sender) {
                normalized.push(sender);
            }
        }
        if normalized.is_empty() {
            return Err(invalid("give at least one sender address"));
        }

        let card_ref = card_ref.map(str::trim);
        if let Some(digits) = card_ref {
            let valid =
                (2..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
            if !valid {
                return Err(invalid(format!(
                    "card digits must be the 2 to 4 digits the email shows after the mask, \
                     got {digits:?}"
                )));
            }
        }

        Ok(Self {
            bank: bank.to_string(),
            senders: normalized,
            card_ref: card_ref.map(str::to_string),
        })
    }

    pub fn senders(&self) -> &[String] {
        &self.senders
    }

    pub fn label(&self) -> String {
        label(&self.bank, self.card_ref.as_deref())
    }
}

fn invalid(message: impl Into<String>) -> LedgerError {
    LedgerError::InvalidCard(message.into())
}

/// Lowercases and sanity-checks a sender address. Not a full email
/// validator: it only catches typos that would make the poller miss mail.
fn normalize_sender(raw: &str) -> Result<String, LedgerError> {
    let sender = raw.trim().to_ascii_lowercase();
    let looks_like_address = match sender.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains('@')
                && !sender.chars().any(|c| c.is_whitespace() || c == ',')
        }
        None => false,
    };
    if looks_like_address {
        Ok(sender)
    } else {
        Err(invalid(format!("{raw:?} is not a sender email address")))
    }
}

/// A `cards` row as stored. `Selectable` builds the column list from the
/// struct, so `created_at`, which nothing reads, can be left out.
#[derive(Queryable, Selectable)]
#[diesel(table_name = cards)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
struct CardRow {
    id: i32,
    bank: String,
    notify_senders: String,
    card_ref: Option<String>,
    cutoff_day: Option<i32>,
    due_day: Option<i32>,
    payable_idr: i64,
}

/// `TryFrom` because decoding the JSON sender list can fail.
impl TryFrom<CardRow> for Card {
    type Error = LedgerError;

    fn try_from(row: CardRow) -> Result<Self, Self::Error> {
        let senders = serde_json::from_str(&row.notify_senders).map_err(|source| {
            LedgerError::CorruptSenders {
                card_id: row.id,
                source,
            }
        })?;
        Ok(Card {
            id: row.id,
            bank: row.bank,
            senders,
            card_ref: row.card_ref,
            cutoff_day: row.cutoff_day,
            due_day: row.due_day,
            payable_idr: row.payable_idr,
        })
    }
}

fn load_cards(conn: &mut SqliteConnection) -> Result<Vec<Card>, LedgerError> {
    cards::table
        .order(cards::id)
        .select(CardRow::as_select())
        .load(conn)?
        .into_iter()
        .map(Card::try_from)
        // Collecting an iterator of `Result`s into one `Result<Vec<_>, _>`
        // stops at the first error.
        .collect()
}

impl Ledger {
    /// Registers a card, refusing one an email could not tell apart from an
    /// existing card (a shared sender with the same card digits, or both
    /// without digits).
    pub fn register_card(&self, new: NewCard) -> Result<Card, LedgerError> {
        let mut conn = self.conn();
        // IMMEDIATE takes the write lock before the clash check, so nothing
        // can insert a clashing card between the check and the insert.
        conn.immediate_transaction(|conn| {
            let existing = load_cards(conn)?;
            let clash = existing.iter().find(|card| {
                card.card_ref == new.card_ref
                    && card.senders.iter().any(|s| new.senders.contains(s))
            });
            if let Some(card) = clash {
                return Err(LedgerError::CardConflict(card.label()));
            }

            let senders_json =
                serde_json::to_string(&new.senders).expect("a list of strings always serializes");
            let row = diesel::insert_into(cards::table)
                .values((
                    cards::bank.eq(&new.bank),
                    cards::notify_senders.eq(senders_json),
                    cards::card_ref.eq(new.card_ref.as_deref()),
                    cards::created_at.eq(Utc::now().naive_utc()),
                ))
                .returning(CardRow::as_returning())
                .get_result(conn)
                .map_err(|err| match err {
                    // UNIQUE (bank, card_ref): same card under other senders.
                    DatabaseError(DatabaseErrorKind::UniqueViolation, _) => {
                        LedgerError::DuplicateCard(new.label())
                    }
                    other => other.into(),
                })?;
            Card::try_from(row)
        })
    }

    /// Every registered card, oldest first.
    pub fn cards(&self) -> Result<Vec<Card>, LedgerError> {
        load_cards(&mut self.conn())
    }

    /// The card an email belongs to: its sender, plus the card digits it
    /// shows (`None` if it shows none).
    pub fn find_card(
        &self,
        sender: &str,
        card_ref: Option<&str>,
    ) -> Result<Option<Card>, LedgerError> {
        let sender = sender.trim().to_ascii_lowercase();
        let cards = load_cards(&mut self.conn())?;
        // A handful of cards, so filtering in Rust beats querying into JSON.
        // The registration invariant means at most one can match.
        Ok(cards
            .into_iter()
            .find(|card| card.card_ref.as_deref() == card_ref && card.senders.contains(&sender)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::test_ledger;

    const LIVIN: &str = "noreply.livin@bankmandiri.co.id";
    const CIMB: &str = "creditcard.notification@cimbniaga.co.id";

    fn new_card(bank: &str, senders: &[&str], card_ref: Option<&str>) -> NewCard {
        NewCard::new(bank, senders, card_ref).unwrap()
    }

    fn invalid_message(result: Result<NewCard, LedgerError>) -> String {
        match result {
            Err(LedgerError::InvalidCard(message)) => message,
            other => panic!("expected InvalidCard, got {other:?}"),
        }
    }

    #[test]
    fn new_card_normalizes_input() {
        let card = new_card(
            "  CIMB Niaga ",
            &[" CreditCard.Notification@CIMBNiaga.co.id", CIMB],
            Some(" 19 "),
        );
        assert_eq!(card.label(), "CIMB Niaga •19");
        assert_eq!(card.senders(), [CIMB]);
    }

    #[test]
    fn new_card_rejects_bad_bank_names() {
        assert!(invalid_message(NewCard::new(" ", &[LIVIN], None)).contains("empty"));
        let long = "B".repeat(MAX_BANK_LEN + 1);
        assert!(invalid_message(NewCard::new(&long, &[LIVIN], None)).contains("longer"));
        let msg = invalid_message(NewCard::new("Mandiri a@b.com,", &[LIVIN], None));
        assert!(msg.contains("no spaces"), "{msg}");
    }

    #[test]
    fn new_card_rejects_bad_senders() {
        for bad in [
            "",
            "bankmandiri.co.id",
            "@bank.co.id",
            "a@bank",
            "a@.co",
            "a@b.co.",
            "a@b@c.co",
            "a b@c.co",
        ] {
            assert!(
                NewCard::new("Mandiri", &[bad], None).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(invalid_message(NewCard::new("Mandiri", &[], None)).contains("at least one"));
    }

    #[test]
    fn new_card_rejects_bad_digits() {
        for bad in ["1", "12345", "XX19", "12a4", ""] {
            assert!(
                NewCard::new("Mandiri", &[LIVIN], Some(bad)).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn registers_and_lists_cards() {
        let ledger = test_ledger();
        let mandiri = ledger
            .register_card(new_card("Mandiri", &[LIVIN], Some("7078")))
            .unwrap();
        assert_eq!(mandiri.label(), "Mandiri •7078");
        assert_eq!(mandiri.payable_idr, 0);
        assert_eq!(mandiri.cutoff_day, None);
        ledger
            .register_card(new_card("CIMB Niaga", &[CIMB], Some("19")))
            .unwrap();

        let labels: Vec<String> = ledger.cards().unwrap().iter().map(Card::label).collect();
        assert_eq!(labels, ["Mandiri •7078", "CIMB Niaga •19"]);
    }

    #[test]
    fn finds_card_by_sender_and_digits() {
        let ledger = test_ledger();
        let first = ledger
            .register_card(new_card("Mandiri", &[LIVIN], Some("7078")))
            .unwrap();
        // Livin' sends one template for every card; the digits pick the card.
        let second = ledger
            .register_card(new_card("Mandiri", &[LIVIN], Some("1234")))
            .unwrap();

        let found = |sender, digits| ledger.find_card(sender, digits).unwrap().map(|c| c.id);
        assert_eq!(found(LIVIN, Some("7078")), Some(first.id));
        assert_eq!(
            found("NoReply.Livin@BankMandiri.co.id ", Some("1234")),
            Some(second.id)
        );
        assert_eq!(found(LIVIN, Some("9999")), None);
        assert_eq!(found(LIVIN, None), None);
        assert_eq!(found(CIMB, Some("7078")), None);
    }

    #[test]
    fn finds_card_without_digits() {
        let ledger = test_ledger();
        let card = ledger
            .register_card(new_card("Jenius", &["jenius_noreply@smbci.com"], None))
            .unwrap();
        let found = ledger.find_card("jenius_noreply@smbci.com", None).unwrap();
        assert_eq!(found.map(|c| c.id), Some(card.id));
    }

    #[test]
    fn refuses_card_an_email_could_not_tell_apart() {
        let ledger = test_ledger();
        ledger
            .register_card(new_card("Mandiri", &[LIVIN], Some("7078")))
            .unwrap();
        // Another bank name, but one shared sender and the same digits.
        let result = ledger.register_card(new_card("Bank Mandiri", &[CIMB, LIVIN], Some("7078")));
        assert!(
            matches!(&result, Err(LedgerError::CardConflict(label)) if label == "Mandiri •7078"),
            "{result:?}"
        );

        ledger
            .register_card(new_card("Jenius", &[CIMB], None))
            .unwrap();
        let result = ledger.register_card(new_card("Other", &[CIMB], None));
        assert!(
            matches!(result, Err(LedgerError::CardConflict(_))),
            "{result:?}"
        );
        assert_eq!(ledger.cards().unwrap().len(), 2);
    }

    #[test]
    fn same_card_under_other_senders_is_a_duplicate() {
        let ledger = test_ledger();
        ledger
            .register_card(new_card("Mandiri", &[LIVIN], Some("7078")))
            .unwrap();
        let result = ledger.register_card(new_card(
            "Mandiri",
            &["mandiricare.onlinenotif@bankmandiri.co.id"],
            Some("7078"),
        ));
        assert!(
            matches!(&result, Err(LedgerError::DuplicateCard(label)) if label == "Mandiri •7078"),
            "{result:?}"
        );
    }
}
