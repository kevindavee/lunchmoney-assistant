//! Free-text messages: the owner's words go to the `IntentParser`, and its
//! proposed actions come back as replies or as proposals with Confirm and
//! Cancel buttons. Nothing is written until Confirm (AGENTS.md rule 3).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use teloxide::prelude::*;
use teloxide::types::{ChatAction, InlineKeyboardButton, InlineKeyboardMarkup};

use super::{no_parser_warnings, register_reply};
use crate::ledger::{Ledger, NewCard};
use crate::llm::{IntentParser, IntentRequest, ProposedAction};

/// A validated write waiting for the owner's Confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingWrite {
    RegisterCard(NewCard),
}

/// Proposals waiting for a tap, by ID. Kept in memory: after a restart the
/// buttons say the proposal expired and the owner sends the message again.
/// Only the ledger writes to the database, and nothing here needs to last.
#[derive(Default)]
pub struct Proposals {
    // `AtomicU64` can be incremented through `&self` without a lock.
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingWrite>>,
}

impl Proposals {
    fn add(&self, write: PendingWrite) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, write);
        id
    }

    /// Removes and returns the proposal, so a second tap cannot run it twice.
    fn take(&self, id: u64) -> Option<PendingWrite> {
        self.lock().remove(&id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, PendingWrite>> {
        self.pending.lock().expect("proposals lock poisoned")
    }
}

/// What the bot says to anything outside its job. Fixed text, so an
/// off-topic request never gets an answer written by the model.
const OUT_OF_SCOPE: &str = "I'm your expense assistant, so I can only help with your \
     cards and expenses: registering cards for now, with charges, payments and \
     balances coming soon. See /help for what I can do.";

/// Longest model reply sent as is, in characters. In-scope replies are a
/// sentence or two; anything longer is almost always the model answering
/// something it should have declined, so it is replaced with `OUT_OF_SCOPE`.
const MAX_REPLY_CHARS: usize = 400;

/// One message to send, with Confirm/Cancel buttons for proposal `confirm`.
#[derive(Debug, PartialEq, Eq)]
struct Outgoing {
    text: String,
    confirm: Option<u64>,
}

impl Outgoing {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            confirm: None,
        }
    }
}

/// Turns the model's actions into messages, validating each write and
/// storing it as a proposal.
fn plan_messages(actions: Vec<ProposedAction>, proposals: &Proposals) -> Vec<Outgoing> {
    let mut out = Vec::new();
    let mut out_of_scope = false;
    for action in actions {
        match action {
            ProposedAction::Reply { text } => {
                let text = text.trim();
                if text.chars().count() > MAX_REPLY_CHARS {
                    tracing::warn!(
                        chars = text.chars().count(),
                        "dropped a long model reply as out of scope"
                    );
                    out_of_scope = true;
                } else if !text.is_empty() {
                    out.push(Outgoing::text(text));
                }
            }
            ProposedAction::OutOfScope => out_of_scope = true,
            ProposedAction::RegisterCard {
                bank,
                senders,
                card_ref,
            } => {
                // `Vec<String>` to `Vec<&str>`: borrow each string as a slice.
                let senders: Vec<&str> = senders.iter().map(String::as_str).collect();
                match NewCard::new(&bank, &senders, card_ref.as_deref()) {
                    Ok(new) => {
                        let text = format!(
                            "Register {}?\nSenders: {}{}",
                            new.label(),
                            new.senders().join(", "),
                            no_parser_warnings(new.senders())
                        );
                        let id = proposals.add(PendingWrite::RegisterCard(new));
                        out.push(Outgoing {
                            text,
                            confirm: Some(id),
                        });
                    }
                    Err(err) => out.push(Outgoing::text(format!(
                        "⚠️ I couldn't use that card: {err}"
                    ))),
                }
            }
        }
    }
    // Once, however many actions were out of scope.
    if out_of_scope {
        out.push(Outgoing::text(OUT_OF_SCOPE));
    }
    if out.is_empty() {
        out.push(Outgoing::text(
            "Sorry, I didn't catch that. Could you say it another way?",
        ));
    }
    out
}

const CONFIRM: &str = "confirm";
const CANCEL: &str = "cancel";

fn confirm_keyboard(id: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("✅ Confirm", format!("{CONFIRM}:{id}")),
        InlineKeyboardButton::callback("✖️ Cancel", format!("{CANCEL}:{id}")),
    ]])
}

/// "confirm:3" -> (true, 3); "cancel:3" -> (false, 3).
fn parse_callback(data: &str) -> Option<(bool, u64)> {
    let (choice, id) = data.split_once(':')?;
    let confirmed = match choice {
        CONFIRM => true,
        CANCEL => false,
        _ => return None,
    };
    Some((confirmed, id.parse().ok()?))
}

/// What a tap on a proposal's button does; returns the result to show.
fn resolve(ledger: &Ledger, proposals: &Proposals, confirmed: bool, id: u64) -> String {
    match (confirmed, proposals.take(id)) {
        (_, None) => "This proposal has expired. Please send your message again.".into(),
        (false, Some(_)) => "✖️ Cancelled.".into(),
        (true, Some(PendingWrite::RegisterCard(new))) => register_reply(ledger, new),
    }
}

pub(super) async fn answer_text(
    bot: Bot,
    msg: Message,
    ledger: Arc<Ledger>,
    intent: Arc<dyn IntentParser>,
    proposals: Arc<Proposals>,
) -> anyhow::Result<()> {
    let Some(text) = msg.text() else {
        return Ok(());
    };
    // Shows "typing…" while the model thinks; a reply takes a few seconds.
    bot.send_chat_action(msg.chat.id, ChatAction::Typing)
        .await?;

    let cards = match ledger.cards() {
        Ok(cards) => cards,
        Err(err) => {
            tracing::error!(error = %err, "loading cards failed");
            bot.send_message(msg.chat.id, format!("⚠️ Could not read cards: {err}"))
                .await?;
            return Ok(());
        }
    };
    let request = IntentRequest {
        message: text.to_string(),
        cards,
    };
    let actions = match intent.parse(request).await {
        Ok(actions) => actions,
        Err(err) => {
            // This message is the alert the plan asks for when the model fails.
            tracing::error!(error = %err, "intent parser failed");
            let reply = format!(
                "⚠️ The language model failed: {err}\n\
                 Slash commands still work; see /help."
            );
            bot.send_message(msg.chat.id, reply).await?;
            return Ok(());
        }
    };

    for outgoing in plan_messages(actions, &proposals) {
        let request = bot.send_message(msg.chat.id, outgoing.text);
        match outgoing.confirm {
            Some(id) => request.reply_markup(confirm_keyboard(id)).await?,
            None => request.await?,
        };
    }
    Ok(())
}

pub(super) async fn answer_callback(
    bot: Bot,
    query: CallbackQuery,
    ledger: Arc<Ledger>,
    proposals: Arc<Proposals>,
) -> anyhow::Result<()> {
    let result = query
        .data
        .as_deref()
        .and_then(parse_callback)
        .map(|(confirmed, id)| resolve(&ledger, &proposals, confirmed, id));
    // Telegram shows a spinner on the button until the query is answered.
    bot.answer_callback_query(query.id.clone()).await?;

    let (Some(result), Some(message)) = (result, query.regular_message()) else {
        return Ok(());
    };
    // Keep the proposal for context and append the outcome. Editing without
    // `reply_markup` removes the buttons, so they cannot be tapped again.
    let text = format!("{}\n\n{result}", message.text().unwrap_or_default());
    bot.edit_message_text(message.chat.id, message.id, text)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::test_ledger;

    const LIVIN: &str = "noreply.livin@bankmandiri.co.id";

    fn register(card_ref: Option<&str>) -> ProposedAction {
        ProposedAction::RegisterCard {
            bank: "Mandiri".into(),
            senders: vec![LIVIN.into()],
            card_ref: card_ref.map(str::to_string),
        }
    }

    #[test]
    fn replies_pass_through() {
        let proposals = Proposals::default();
        let actions = vec![
            ProposedAction::Reply {
                text: "Halo!".into(),
            },
            ProposedAction::Reply { text: "  ".into() },
        ];
        assert_eq!(
            plan_messages(actions, &proposals),
            vec![Outgoing::text("Halo!")]
        );
    }

    #[test]
    fn out_of_scope_gets_the_fixed_message_once() {
        let out = plan_messages(
            vec![ProposedAction::OutOfScope, ProposedAction::OutOfScope],
            &Proposals::default(),
        );
        assert_eq!(out, vec![Outgoing::text(OUT_OF_SCOPE)]);
    }

    #[test]
    fn long_reply_is_replaced_with_the_fixed_message() {
        let essay = "Paris is the capital of France. ".repeat(20);
        let out = plan_messages(
            vec![ProposedAction::Reply { text: essay }],
            &Proposals::default(),
        );
        assert_eq!(out, vec![Outgoing::text(OUT_OF_SCOPE)]);

        let short = "x".repeat(MAX_REPLY_CHARS);
        let out = plan_messages(
            vec![ProposedAction::Reply {
                text: short.clone(),
            }],
            &Proposals::default(),
        );
        assert_eq!(out, vec![Outgoing::text(short)]);
    }

    #[test]
    fn in_scope_actions_survive_next_to_an_out_of_scope_one() {
        let proposals = Proposals::default();
        let out = plan_messages(
            vec![register(Some("7078")), ProposedAction::OutOfScope],
            &proposals,
        );
        assert_eq!(out.len(), 2);
        assert!(out[0].confirm.is_some());
        assert_eq!(out[1], Outgoing::text(OUT_OF_SCOPE));
    }

    #[test]
    fn no_actions_asks_again() {
        let out = plan_messages(vec![], &Proposals::default());
        assert!(out[0].text.starts_with("Sorry"), "{out:?}");
    }

    #[test]
    fn valid_card_becomes_a_proposal() {
        let proposals = Proposals::default();
        let out = plan_messages(vec![register(Some("7078"))], &proposals);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].text,
            format!("Register Mandiri •7078?\nSenders: {LIVIN}")
        );
        let id = out[0].confirm.unwrap();
        assert!(matches!(
            proposals.take(id),
            Some(PendingWrite::RegisterCard(_))
        ));
    }

    #[test]
    fn invalid_card_is_explained_not_proposed() {
        let proposals = Proposals::default();
        let out = plan_messages(vec![register(None)], &proposals);
        assert_eq!(out[0].confirm, None);
        assert!(out[0].text.contains("please add them"), "{out:?}");
        assert!(proposals.lock().is_empty());
    }

    #[test]
    fn confirm_registers_once() {
        let ledger = test_ledger();
        let proposals = Proposals::default();
        let id = plan_messages(vec![register(Some("7078"))], &proposals)[0]
            .confirm
            .unwrap();

        let result = resolve(&ledger, &proposals, true, id);
        assert!(
            result.starts_with("✅ Registered Mandiri •7078"),
            "{result}"
        );
        assert_eq!(ledger.cards().unwrap().len(), 1);
        // A second tap finds nothing to run.
        assert!(resolve(&ledger, &proposals, true, id).contains("expired"));
        assert_eq!(ledger.cards().unwrap().len(), 1);
    }

    #[test]
    fn cancel_writes_nothing() {
        let ledger = test_ledger();
        let proposals = Proposals::default();
        let id = plan_messages(vec![register(Some("7078"))], &proposals)[0]
            .confirm
            .unwrap();
        assert_eq!(resolve(&ledger, &proposals, false, id), "✖️ Cancelled.");
        assert!(ledger.cards().unwrap().is_empty());
    }

    #[test]
    fn parses_callback_data() {
        assert_eq!(parse_callback("confirm:3"), Some((true, 3)));
        assert_eq!(parse_callback("cancel:12"), Some((false, 12)));
        assert_eq!(parse_callback("confirm:x"), None);
        assert_eq!(parse_callback("other:1"), None);
        assert_eq!(parse_callback("confirm"), None);
    }
}
