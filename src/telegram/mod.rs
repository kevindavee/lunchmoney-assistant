//! Telegram bot: command handlers and the owner allowlist.
//!
//! Phase 0 diagnostics (/ping, /gmail) plus /cards to register cards.

use std::sync::Arc;

use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::gmail::{GmailClient, GmailError};
use crate::ledger::{Card, Ledger, NewCard};
use crate::parsers;

/// How many subjects /gmail lists.
const GMAIL_SUBJECT_COUNT: u32 = 5;

/// The Telegram user allowed to talk to the bot. A newtype (a one-field
/// struct) so dptree can inject it by type without confusing it with other
/// `u64` values.
#[derive(Clone, Copy)]
pub struct OwnerId(pub UserId);

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    /// Check that the bot is alive.
    Ping,
    /// List the newest Gmail subjects.
    Gmail,
    /// List cards, or add one: /cards add <bank> <sender>[,<sender>...] [digits]
    Cards(String),
    /// Show the commands.
    Help,
}

/// Builds the update handler tree.
///
/// The first filter drops every update that is not a message from the owner,
/// so no handler below it ever sees anyone else.
pub fn handler() -> UpdateHandler<anyhow::Error> {
    Update::filter_message()
        .filter(is_from_owner)
        .filter_command::<Command>()
        .endpoint(answer_command)
    // Anything that falls through (other users, plain text until Phase 2) is
    // dropped by the dispatcher's default handler in main.rs.
}

fn is_from_owner(msg: Message, owner: OwnerId) -> bool {
    msg.from.as_ref().is_some_and(|user| user.id == owner.0)
}

async fn answer_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    gmail: Arc<GmailClient>,
    ledger: Arc<Ledger>,
) -> anyhow::Result<()> {
    let reply = match cmd {
        Command::Ping => "pong".to_string(),
        Command::Help => Command::descriptions().to_string(),
        Command::Gmail => match gmail.latest_subjects(GMAIL_SUBJECT_COUNT).await {
            Ok(subjects) => format_subjects(&subjects),
            Err(err @ GmailError::InvalidGrant) => {
                tracing::error!(error = %err, "gmail auth failed");
                format!("⚠️ {err}")
            }
            Err(err) => {
                tracing::error!(error = %err, "gmail request failed");
                format!("⚠️ Could not read Gmail: {err}")
            }
        },
        Command::Cards(args) => answer_cards(&ledger, &args),
    };
    bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

fn format_subjects(subjects: &[String]) -> String {
    if subjects.is_empty() {
        return "Inbox is empty.".to_string();
    }
    subjects
        .iter()
        .enumerate()
        .map(|(i, subject)| format!("{}. {subject}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

const CARDS_USAGE: &str = "Usage: /cards add <bank> <sender>[,<sender>...] [digits]\n\
     digits: the 2 to 4 card digits the email shows after the mask; leave out if it shows none.\n\
     Example: /cards add Mandiri noreply.livin@bankmandiri.co.id 7078";

/// What the text after /cards asks for.
#[derive(Debug, PartialEq, Eq)]
enum CardsArgs {
    List,
    Add(NewCard),
}

/// Parses "/cards" or "/cards add <bank> <senders> [digits]". The bank name
/// may have spaces ("CIMB Niaga"), so it is read from the end: digits last
/// (if all digits), then the comma-separated senders, and the rest is the bank.
fn parse_cards_args(args: &str) -> Result<CardsArgs, String> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    match words.first() {
        None => return Ok(CardsArgs::List),
        Some(word) if word.eq_ignore_ascii_case("add") => {
            words.remove(0);
        }
        Some(_) => return Err(CARDS_USAGE.to_string()),
    }

    let card_ref = match words.last() {
        Some(last) if last.bytes().all(|b| b.is_ascii_digit()) => words.pop(),
        _ => None,
    };
    let senders: Vec<&str> = match words.pop() {
        Some(word) if word.contains('@') => word.split(',').filter(|s| !s.is_empty()).collect(),
        _ => return Err(CARDS_USAGE.to_string()),
    };
    let bank = words.join(" ");

    NewCard::new(&bank, &senders, card_ref)
        .map(CardsArgs::Add)
        .map_err(|err| format!("⚠️ {err}\n\n{CARDS_USAGE}"))
}

fn answer_cards(ledger: &Ledger, args: &str) -> String {
    let new = match parse_cards_args(args) {
        Ok(CardsArgs::List) => {
            return match ledger.cards() {
                Ok(cards) => format_cards(&cards),
                Err(err) => {
                    tracing::error!(error = %err, "listing cards failed");
                    format!("⚠️ Could not list cards: {err}")
                }
            };
        }
        Ok(CardsArgs::Add(new)) => new,
        Err(reply) => return reply,
    };

    match ledger.register_card(new) {
        Ok(card) => format_registered(&card),
        Err(err) => {
            tracing::warn!(error = %err, "registering card failed");
            format!("⚠️ Not registered: {err}")
        }
    }
}

fn format_cards(cards: &[Card]) -> String {
    if cards.is_empty() {
        return format!("No cards registered yet.\n\n{CARDS_USAGE}");
    }
    let lines: Vec<String> = cards
        .iter()
        .map(|card| format!("• {}: {}", card.label(), card.senders.join(", ")))
        .collect();
    format!("Registered cards:\n{}", lines.join("\n"))
}

fn format_registered(card: &Card) -> String {
    let mut reply = format!(
        "✅ Registered {} (senders: {}).",
        card.label(),
        card.senders.join(", ")
    );
    for sender in &card.senders {
        if parsers::for_sender(sender).is_none() {
            reply.push_str(&format!(
                "\n⚠️ No parser for {sender} yet, so its emails cannot be read until one is added."
            ));
        }
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVIN: &str = "noreply.livin@bankmandiri.co.id";

    fn add(bank: &str, senders: &[&str], digits: Option<&str>) -> CardsArgs {
        CardsArgs::Add(NewCard::new(bank, senders, digits).unwrap())
    }

    #[test]
    fn cards_without_args_lists() {
        assert_eq!(parse_cards_args(""), Ok(CardsArgs::List));
        assert_eq!(parse_cards_args("   "), Ok(CardsArgs::List));
    }

    #[test]
    fn cards_add_reads_bank_senders_and_digits() {
        assert_eq!(
            parse_cards_args(&format!("add Mandiri {LIVIN} 7078")),
            Ok(add("Mandiri", &[LIVIN], Some("7078")))
        );
        assert_eq!(
            parse_cards_args("ADD CIMB  Niaga creditcard.notification@cimbniaga.co.id 19"),
            Ok(add(
                "CIMB Niaga",
                &["creditcard.notification@cimbniaga.co.id"],
                Some("19")
            ))
        );
        assert_eq!(
            parse_cards_args("add Jenius jenius_noreply@smbci.com"),
            Ok(add("Jenius", &["jenius_noreply@smbci.com"], None))
        );
        assert_eq!(
            parse_cards_args(&format!(
                "add Mandiri {LIVIN},mandiricare.onlinenotif@bankmandiri.co.id 7078"
            )),
            Ok(add(
                "Mandiri",
                &[LIVIN, "mandiricare.onlinenotif@bankmandiri.co.id"],
                Some("7078")
            ))
        );
    }

    #[test]
    fn cards_add_explains_mistakes() {
        for bad in [
            "remove 1",
            "add",
            "add Mandiri",
            "add Mandiri 7078",
            "add a@b.com 7078",
        ] {
            let reply = parse_cards_args(bad).unwrap_err();
            assert!(reply.contains("Usage"), "{bad:?}: {reply}");
        }
        let reply = parse_cards_args(&format!("add Mandiri {LIVIN} 12345")).unwrap_err();
        assert!(reply.contains("2 to 4 digits"), "{reply}");
    }

    #[test]
    fn registers_through_the_command() {
        let ledger = crate::ledger::test_ledger();
        let reply = answer_cards(&ledger, &format!("add Mandiri {LIVIN} 7078"));
        assert_eq!(
            reply,
            format!("✅ Registered Mandiri •7078 (senders: {LIVIN}).")
        );
        assert_eq!(
            answer_cards(&ledger, ""),
            format!("Registered cards:\n• Mandiri •7078: {LIVIN}")
        );
        let reply = answer_cards(&ledger, &format!("add Mandiri {LIVIN} 7078"));
        assert!(reply.starts_with("⚠️ Not registered"), "{reply}");
    }

    #[test]
    fn warns_when_no_parser_reads_a_sender() {
        let ledger = crate::ledger::test_ledger();
        let reply = answer_cards(&ledger, "add BCA notif@bca.co.id 4417");
        assert!(reply.contains("No parser for notif@bca.co.id"), "{reply}");
    }

    #[test]
    fn empty_card_list_shows_usage() {
        assert!(format_cards(&[]).contains("Usage"));
    }

    #[test]
    fn formats_numbered_subjects() {
        let subjects = vec!["First".to_string(), "Second".to_string()];
        assert_eq!(format_subjects(&subjects), "1. First\n2. Second");
    }

    #[test]
    fn formats_empty_inbox() {
        assert_eq!(format_subjects(&[]), "Inbox is empty.");
    }
}
