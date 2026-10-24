//! Configuration, read from environment variables only.
//!
//! Phase 0 only loads what the bot and the Gmail client need. Later phases add
//! Lunch Money, the LLM and Jev settings here.

use std::net::SocketAddr;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    #[error("environment variable {name} is invalid: {reason}")]
    Invalid { name: &'static str, reason: String },
}

/// All settings for the process.
///
/// Deliberately not `Debug`: it holds secrets, and a stray `{:?}` in a log
/// line would leak them.
pub struct Config {
    pub telegram: TelegramConfig,
    pub google: GoogleConfig,
    /// SQLite file. `/data/expense.db` on Fly (the volume), any local path in dev.
    pub database_path: PathBuf,
}

pub struct TelegramConfig {
    pub bot_token: String,
    /// The only Telegram user the bot answers.
    pub owner_id: u64,
    /// `Some` in production (webhook), `None` locally (long polling).
    pub webhook: Option<WebhookConfig>,
}

pub struct WebhookConfig {
    /// Public URL Telegram posts updates to, e.g. `https://app.fly.dev/webhook`.
    pub url: url::Url,
    /// Sent by Telegram in the `X-Telegram-Bot-Api-Secret-Token` header and
    /// checked on every request.
    pub secret: String,
    /// Local address the HTTP server binds to.
    pub listen: SocketAddr,
}

pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

impl Config {
    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the configuration through `lookup`, so tests can supply values
    /// without touching the real environment.
    ///
    /// `impl Fn(&str) -> Option<String>` accepts any closure with that shape;
    /// the compiler generates a version of this function for each one.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        // Treat empty values (`FOO=` in .env) the same as unset.
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let require = |name: &'static str| get(name).ok_or(ConfigError::Missing(name));

        let owner_id = require("TELEGRAM_OWNER_ID")?
            .trim()
            .parse::<u64>()
            .map_err(|e| ConfigError::Invalid {
                name: "TELEGRAM_OWNER_ID",
                reason: e.to_string(),
            })?;

        let webhook = match get("TELEGRAM_WEBHOOK_URL") {
            None => None,
            Some(raw_url) => {
                let url = url::Url::parse(raw_url.trim()).map_err(|e| ConfigError::Invalid {
                    name: "TELEGRAM_WEBHOOK_URL",
                    reason: e.to_string(),
                })?;
                let secret = require("TELEGRAM_WEBHOOK_SECRET")?;
                validate_webhook_secret(&secret)?;
                let port = match get("PORT") {
                    None => 8080,
                    Some(p) => p.trim().parse::<u16>().map_err(|e| ConfigError::Invalid {
                        name: "PORT",
                        reason: e.to_string(),
                    })?,
                };
                Some(WebhookConfig {
                    url,
                    secret,
                    listen: SocketAddr::from(([0, 0, 0, 0], port)),
                })
            }
        };

        Ok(Config {
            telegram: TelegramConfig {
                bot_token: require("TELEGRAM_BOT_TOKEN")?,
                owner_id,
                webhook,
            },
            google: GoogleConfig {
                client_id: require("GOOGLE_CLIENT_ID")?,
                client_secret: require("GOOGLE_CLIENT_SECRET")?,
                refresh_token: require("GOOGLE_REFRESH_TOKEN")?,
            },
            database_path: PathBuf::from(require("DATABASE_PATH")?),
        })
    }
}

/// Telegram only accepts 1-256 characters from `A-Z a-z 0-9 _ -`.
/// teloxide panics on anything else, so we reject it here with a clear error.
fn validate_webhook_secret(secret: &str) -> Result<(), ConfigError> {
    let valid_len = (1..=256).contains(&secret.len());
    let valid_chars = secret
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid_len && valid_chars {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            name: "TELEGRAM_WEBHOOK_SECRET",
            reason: "must be 1-256 characters of A-Z, a-z, 0-9, _ or -".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_env() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("TELEGRAM_BOT_TOKEN", "123:abc"),
            ("TELEGRAM_OWNER_ID", "42"),
            ("GOOGLE_CLIENT_ID", "cid"),
            ("GOOGLE_CLIENT_SECRET", "csecret"),
            ("GOOGLE_REFRESH_TOKEN", "rtoken"),
            ("DATABASE_PATH", "/tmp/expense.db"),
        ])
    }

    fn load(env: &HashMap<&'static str, &'static str>) -> Result<Config, ConfigError> {
        Config::from_lookup(|name| env.get(name).map(|v| v.to_string()))
    }

    // `Config` is not `Debug`, so `unwrap_err` is unavailable; this does the same.
    fn expect_err(result: Result<Config, ConfigError>) -> ConfigError {
        match result {
            Ok(_) => panic!("expected a config error"),
            Err(e) => e,
        }
    }

    #[test]
    fn loads_polling_config_without_webhook_url() {
        let config = load(&base_env()).unwrap();
        assert_eq!(config.telegram.owner_id, 42);
        assert!(config.telegram.webhook.is_none());
        assert_eq!(config.google.refresh_token, "rtoken");
        assert_eq!(config.database_path, PathBuf::from("/tmp/expense.db"));
    }

    #[test]
    fn loads_webhook_config() {
        let mut env = base_env();
        env.insert("TELEGRAM_WEBHOOK_URL", "https://example.fly.dev/webhook");
        env.insert("TELEGRAM_WEBHOOK_SECRET", "s3cret_value-1");
        env.insert("PORT", "9000");
        let webhook = load(&env).unwrap().telegram.webhook.unwrap();
        assert_eq!(webhook.url.path(), "/webhook");
        assert_eq!(webhook.listen.port(), 9000);
    }

    #[test]
    fn missing_variable_is_reported_by_name() {
        let mut env = base_env();
        env.remove("GOOGLE_REFRESH_TOKEN");
        assert_eq!(
            expect_err(load(&env)),
            ConfigError::Missing("GOOGLE_REFRESH_TOKEN")
        );
    }

    #[test]
    fn empty_value_counts_as_missing() {
        let mut env = base_env();
        env.insert("TELEGRAM_BOT_TOKEN", "");
        assert_eq!(
            expect_err(load(&env)),
            ConfigError::Missing("TELEGRAM_BOT_TOKEN")
        );
    }

    #[test]
    fn non_numeric_owner_id_is_invalid() {
        let mut env = base_env();
        env.insert("TELEGRAM_OWNER_ID", "@kevin");
        assert!(matches!(
            expect_err(load(&env)),
            ConfigError::Invalid {
                name: "TELEGRAM_OWNER_ID",
                ..
            }
        ));
    }

    #[test]
    fn webhook_requires_secret() {
        let mut env = base_env();
        env.insert("TELEGRAM_WEBHOOK_URL", "https://example.fly.dev/webhook");
        assert_eq!(
            expect_err(load(&env)),
            ConfigError::Missing("TELEGRAM_WEBHOOK_SECRET")
        );
    }

    #[test]
    fn webhook_secret_with_bad_characters_is_rejected() {
        let mut env = base_env();
        env.insert("TELEGRAM_WEBHOOK_URL", "https://example.fly.dev/webhook");
        env.insert("TELEGRAM_WEBHOOK_SECRET", "has spaces!");
        assert!(matches!(
            expect_err(load(&env)),
            ConfigError::Invalid {
                name: "TELEGRAM_WEBHOOK_SECRET",
                ..
            }
        ));
    }
}
