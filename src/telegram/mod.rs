//! Telegram bot: command handlers and the owner allowlist.
//!
//! Phase 0 has two diagnostic commands: /ping and /gmail.

use std::sync::Arc;

use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;

use crate::gmail::{GmailClient, GmailError};

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

#[cfg(test)]
mod tests {
    use super::*;

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
