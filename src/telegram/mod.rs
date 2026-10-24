//! Telegram bot: command handlers, free-text chat and the owner allowlist.
//!
//! Commands are shortcuts and the fallback when the model is down; anything
//! else the owner writes goes to the `IntentParser` (see `chat`).

mod chat;

pub use chat::Proposals;

use std::sync::Arc;

use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::gmail::{GmailClient, GmailError};
use crate::ledger::{Card, Ledger, NewCard};
use crate::llm::IntentParser;
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
    /// List cards, or add one in your own words: /cards my CIMB card ends in 19
    Cards(String),
    /// Show the commands.
    Help,
}

/// Builds the update handler tree.
///
/// Each branch starts by dropping updates that are not from the owner, so no
/// handler below ever sees anyone else. A branch whose filter fails passes
/// the update to the next one; anything left over (other users, photos) is
/// dropped by the dispatcher's default handler in main.rs.
pub fn handler() -> UpdateHandler<anyhow::Error> {
    let messages = Update::filter_message()
        .filter(is_from_owner)
        .branch(
            dptree::entry()
                .filter_command::<Command>()
                .endpoint(answer_command),
        )
        .branch(dptree::filter(is_bare_unknown_command).endpoint(answer_unknown_command))
        .branch(dptree::filter(|msg: Message| msg.text().is_some()).endpoint(chat::answer_text));
    let buttons = Update::filter_callback_query()
        .filter(|query: CallbackQuery, owner: OwnerId| query.from.id == owner.0)
        .endpoint(chat::answer_callback);
    dptree::entry().branch(messages).branch(buttons)
}

fn is_from_owner(msg: Message, owner: OwnerId) -> bool {
    msg.from.as_ref().is_some_and(|user| user.id == owner.0)
}

/// A lone "/word" the command parser rejected, such as a typo. A command
/// followed by text ("/help how do I add a card?") is not caught here; it
/// falls through to the chat, which reads the command as a hint.
fn is_bare_unknown_command(msg: Message) -> bool {
    msg.text()
        .map(str::trim)
        .is_some_and(|text| text.starts_with('/') && !text.contains(char::is_whitespace))
}

async fn answer_unknown_command(bot: Bot, msg: Message) -> anyhow::Result<()> {
    bot.send_message(msg.chat.id, "I don't know that command. See /help.")
        .await?;
    Ok(())
}

async fn answer_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    gmail: Arc<GmailClient>,
    ledger: Arc<Ledger>,
    intent: Arc<dyn IntentParser>,
    proposals: Arc<Proposals>,
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
        Command::Cards(args) => match parse_cards_args(&args) {
            CardsArgs::List => list_cards_reply(&ledger),
            CardsArgs::Add(new) => register_reply(&ledger, new),
            // The whole message, "/cards" included, goes to the model.
            CardsArgs::Chat => {
                return chat::answer_text(bot, msg, ledger, intent, proposals).await;
            }
        },
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

const CARDS_HELP: &str = "Tell me about a card in your own words, e.g.\n\
     /cards my CIMB Niaga card ends in 19\n\n\
     Without the model, the exact form also works:\n\
     /cards add <bank> <sender>[,<sender>...] [digits]";

/// What the text after /cards asks for.
#[derive(Debug, PartialEq, Eq)]
enum CardsArgs {
    /// Nothing after /cards.
    List,
    /// The exact form, "add <bank> <senders> [digits]", fully valid. It
    /// needs no model, so it still works when the model is down, and it
    /// registers without a Confirm step: typing it out is the confirmation.
    Add(NewCard),
    /// Anything else goes to the model.
    Chat,
}

/// Reads the text after /cards. For the exact form, the bank name may have
/// spaces ("CIMB Niaga"), so it is read from the end: digits last (if all
/// digits), then the comma-separated senders, and the rest is the bank.
fn parse_cards_args(args: &str) -> CardsArgs {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    match words.first() {
        None => return CardsArgs::List,
        Some(word) if word.eq_ignore_ascii_case("add") => {
            words.remove(0);
        }
        Some(_) => return CardsArgs::Chat,
    }

    let card_ref = match words.last() {
        Some(last) if last.bytes().all(|b| b.is_ascii_digit()) => words.pop(),
        _ => None,
    };
    let senders: Vec<&str> = match words.pop() {
        Some(word) if word.contains('@') => word.split(',').filter(|s| !s.is_empty()).collect(),
        _ => return CardsArgs::Chat,
    };
    let bank = words.join(" ");

    // An exact-looking form that fails validation also goes to the model,
    // which proposes the card; code then explains what is wrong with it.
    NewCard::new(&bank, &senders, card_ref).map_or(CardsArgs::Chat, CardsArgs::Add)
}

fn list_cards_reply(ledger: &Ledger) -> String {
    match ledger.cards() {
        Ok(cards) => format_cards(&cards),
        Err(err) => {
            tracing::error!(error = %err, "listing cards failed");
            format!("⚠️ Could not list cards: {err}")
        }
    }
}

/// Registers a validated card and says how it went. Used by both the exact
/// /cards form and the chat's Confirm button.
fn register_reply(ledger: &Ledger, new: NewCard) -> String {
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
        return format!("No cards registered yet.\n\n{CARDS_HELP}");
    }
    let lines: Vec<String> = cards
        .iter()
        .map(|card| format!("• {}: {}", card.label(), card.senders.join(", ")))
        .collect();
    format!("Registered cards:\n{}", lines.join("\n"))
}

fn format_registered(card: &Card) -> String {
    format!(
        "✅ Registered {} (senders: {}).{}",
        card.label(),
        card.senders.join(", "),
        no_parser_warnings(&card.senders)
    )
}

/// A warning line for each sender no parser reads, or "" if all have one.
fn no_parser_warnings(senders: &[String]) -> String {
    senders
        .iter()
        .filter(|sender| parsers::for_sender(sender).is_none())
        .map(|sender| {
            format!(
                "\n⚠️ No parser for {sender} yet, so its emails cannot be read until one is added."
            )
        })
        .collect()
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
        assert_eq!(parse_cards_args(""), CardsArgs::List);
        assert_eq!(parse_cards_args("   "), CardsArgs::List);
    }

    #[test]
    fn cards_add_reads_bank_senders_and_digits() {
        assert_eq!(
            parse_cards_args(&format!("add Mandiri {LIVIN} 7078")),
            add("Mandiri", &[LIVIN], Some("7078"))
        );
        assert_eq!(
            parse_cards_args("ADD CIMB  Niaga creditcard.notification@cimbniaga.co.id 19"),
            add(
                "CIMB Niaga",
                &["creditcard.notification@cimbniaga.co.id"],
                Some("19")
            )
        );
        assert_eq!(
            parse_cards_args("add Other Bank notif@bank.example"),
            add("Other Bank", &["notif@bank.example"], None)
        );
        assert_eq!(
            parse_cards_args(&format!(
                "add Mandiri {LIVIN},mandiricare.onlinenotif@bankmandiri.co.id 7078"
            )),
            add(
                "Mandiri",
                &[LIVIN, "mandiricare.onlinenotif@bankmandiri.co.id"],
                Some("7078")
            )
        );
    }

    #[test]
    fn anything_but_the_exact_form_goes_to_the_model() {
        for text in [
            "my CIMB Niaga card ends in 19",
            "add my Mandiri card, ends in 7078",
            "remove 1",
            "add",
            "add Mandiri",
            "add Mandiri 7078",
            "add a@b.com 7078",
            // Exact form, but Mandiri emails show 4 digits.
            "add Mandiri noreply.livin@bankmandiri.co.id 78",
        ] {
            assert_eq!(parse_cards_args(text), CardsArgs::Chat, "{text:?}");
        }
    }

    #[test]
    fn registers_through_the_exact_form() {
        let ledger = crate::ledger::test_ledger();
        let CardsArgs::Add(new) = parse_cards_args(&format!("add Mandiri {LIVIN} 7078")) else {
            panic!("expected the exact form");
        };
        assert_eq!(
            register_reply(&ledger, new.clone()),
            format!("✅ Registered Mandiri •7078 (senders: {LIVIN}).")
        );
        assert_eq!(
            list_cards_reply(&ledger),
            format!("Registered cards:\n• Mandiri •7078: {LIVIN}")
        );
        let reply = register_reply(&ledger, new);
        assert!(reply.starts_with("⚠️ Not registered"), "{reply}");
    }

    #[test]
    fn warns_when_no_parser_reads_a_sender() {
        let ledger = crate::ledger::test_ledger();
        let new = NewCard::new("BCA", &["notif@bca.co.id"], Some("4417")).unwrap();
        let reply = register_reply(&ledger, new);
        assert!(reply.contains("No parser for notif@bca.co.id"), "{reply}");
    }

    #[test]
    fn empty_card_list_explains_how_to_add() {
        assert!(format_cards(&[]).contains("in your own words"));
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
