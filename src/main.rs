use std::sync::Arc;

use anyhow::Context;
use teloxide::prelude::*;
use teloxide::update_listeners::{Polling, webhooks};
use tracing_subscriber::EnvFilter;

use expense_assistant::config::Config;
use expense_assistant::db;
use expense_assistant::gmail::GmailClient;
use expense_assistant::ledger::Ledger;
use expense_assistant::telegram::{self, OwnerId};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Local convenience: load `.env` into the environment if it exists.
    // Variables already set (e.g. Fly secrets) win over the file. Production
    // has no `.env`, so "not found" is fine; a malformed file is an error.
    // Runs before logging starts so RUST_LOG from `.env` takes effect.
    // `if let ... && cond` (a "let chain", Rust 2024) only enters the block
    // when the pattern matches and the condition holds.
    if let Err(err) = dotenvy::dotenv()
        && !err.not_found()
    {
        return Err(err).context("reading .env");
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().context("loading configuration")?;

    // Opening runs any pending migrations. The ledger owns the connection;
    // nothing else touches the database.
    let conn = db::open(&config.database_path)
        .with_context(|| format!("opening database at {}", config.database_path.display()))?;
    let ledger = Arc::new(Ledger::new(conn));
    tracing::info!(path = %config.database_path.display(), "database ready");

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;
    // `Arc` lets every handler share one client without copying it.
    let gmail = Arc::new(GmailClient::new(http, &config.google));
    let owner = OwnerId(UserId(config.telegram.owner_id));

    let bot = Bot::new(&config.telegram.bot_token);

    let mut dispatcher = Dispatcher::builder(bot.clone(), telegram::handler())
        .dependencies(dptree::deps![gmail, ledger, owner])
        // Silently drop updates no handler took: other users, plain text.
        .default_handler(|_| async {})
        .enable_ctrlc_handler()
        .build();

    match config.telegram.webhook {
        Some(webhook) => {
            tracing::info!(url = %webhook.url, listen = %webhook.listen, "starting in webhook mode");
            // teloxide registers the webhook with Telegram and rejects any
            // request whose X-Telegram-Bot-Api-Secret-Token header does not
            // match `secret`.
            let options =
                webhooks::Options::new(webhook.listen, webhook.url).secret_token(webhook.secret);
            let listener = webhooks::axum(bot, options)
                .await
                .context("setting up Telegram webhook")?;
            dispatcher
                .dispatch_with_listener(
                    listener,
                    LoggingErrorHandler::with_custom_text("error from webhook listener"),
                )
                .await;
        }
        None => {
            tracing::info!("starting in long-polling mode");
            // teloxide's default polling deletes any webhook, which would
            // silently disconnect the deployed bot. Refuse instead.
            let info = bot
                .get_webhook_info()
                .await
                .context("checking Telegram webhook")?;
            if let Some(url) = info.url {
                anyhow::bail!(
                    "a webhook is registered at {url}, so this token is in use by a deployed bot; \
                     use a separate dev bot token for local polling"
                );
            }
            let listener = Polling::builder(bot)
                .timeout(std::time::Duration::from_secs(10))
                .build();
            dispatcher
                .dispatch_with_listener(
                    listener,
                    LoggingErrorHandler::with_custom_text("error from polling listener"),
                )
                .await;
        }
    }

    Ok(())
}
