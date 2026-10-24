//! `IntentParser` backed by the `claude` CLI and the Claude subscription:
//! `claude -p --output-format json --json-schema ...` with every tool off.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::LlmConfig;

use super::{
    BoxFuture, INTENT_SCHEMA, INTENT_SYSTEM_PROMPT, IntentParser, IntentRequest, LlmError,
    ProposedAction, actions_from_json, intent_prompt,
};

/// A reply usually takes a few seconds; past this, something is wrong.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

/// `claude --version` answers at once; this only guards against a hang.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variables the CLI may see. Everything else is cleared so the
/// model's process never holds the bot's secrets, and an `ANTHROPIC_API_KEY`
/// in the environment cannot silently switch it to pay-per-use billing.
const PASSTHROUGH_ENV: [&str; 6] = ["PATH", "HOME", "USER", "LANG", "TMPDIR", "SHELL"];

pub struct ClaudeCli {
    program: PathBuf,
    oauth_token: Option<String>,
    /// Which account folder the CLI uses (`CLAUDE_CONFIG_DIR`).
    config_dir: Option<PathBuf>,
    /// An empty folder to run in, so the CLI picks up no CLAUDE.md or
    /// project settings from wherever the bot was started.
    work_dir: PathBuf,
    timeout: Duration,
}

impl ClaudeCli {
    pub fn new(config: &LlmConfig) -> Self {
        Self {
            program: PathBuf::from("claude"),
            oauth_token: config.oauth_token.clone(),
            config_dir: config.config_dir.clone(),
            work_dir: std::env::temp_dir().join("expense-assistant-llm"),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Runs `program` instead of `claude`; tests use a fake script.
    #[cfg(test)]
    fn with_program(program: PathBuf, work_dir: PathBuf, timeout: Duration) -> Self {
        Self {
            program,
            oauth_token: Some("test-token".into()),
            config_dir: Some(PathBuf::from("/accounts/work")),
            work_dir,
            timeout,
        }
    }

    /// Checks that the CLI starts, and returns its version ("2.1.287
    /// (Claude Code)"). Run once at startup so a missing or broken CLI shows
    /// up then, not on the first message. It does not check the login:
    /// `--version` works signed out.
    pub async fn check(&self) -> Result<String, LlmError> {
        let mut command = self.command().await?;
        command.arg("--version").stdin(Stdio::null());
        let output = tokio::time::timeout(CHECK_TIMEOUT, command.output())
            .await
            .map_err(|_| LlmError::Timeout(CHECK_TIMEOUT.as_secs()))?
            .map_err(|source| self.spawn_error(source))?;
        if !output.status.success() {
            return Err(LlmError::Exit {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// The CLI with a clean environment, run in the empty work folder.
    async fn command(&self) -> Result<Command, LlmError> {
        tokio::fs::create_dir_all(&self.work_dir).await?;
        let mut command = Command::new(&self.program);
        command
            .current_dir(&self.work_dir)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If a timeout drops the child, kill it rather than leave it
            // running in the background.
            .kill_on_drop(true);
        for name in PASSTHROUGH_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(token) = &self.oauth_token {
            command.env("CLAUDE_CODE_OAUTH_TOKEN", token);
        }
        if let Some(dir) = &self.config_dir {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        Ok(command)
    }

    fn spawn_error(&self, source: std::io::Error) -> LlmError {
        LlmError::Spawn {
            program: self.program.display().to_string(),
            source,
        }
    }

    /// Sends `prompt` on stdin and returns the JSON the model produced.
    async fn run(
        &self,
        system: &str,
        prompt: &str,
        schema: &str,
    ) -> Result<serde_json::Value, LlmError> {
        let mut command = self.command().await?;
        command.args(cli_args(system, schema)).stdin(Stdio::piped());

        let mut child = command.spawn().map_err(|source| self.spawn_error(source))?;
        // The prompt is a few KB, well under the pipe buffer, so writing it
        // all before reading the output cannot deadlock.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin.write_all(prompt.as_bytes()).await?;
        // Dropping stdin closes it, which tells the CLI the prompt is done.
        drop(stdin);

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| LlmError::Timeout(self.timeout.as_secs()))??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() && !output.status.success() {
            return Err(LlmError::Exit {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        // On failure the CLI still prints its JSON result with `is_error`,
        // which carries a clearer message than the exit status.
        structured_output(&stdout)
    }
}

impl IntentParser for ClaudeCli {
    fn parse(
        &self,
        request: IntentRequest,
    ) -> BoxFuture<'_, Result<Vec<ProposedAction>, LlmError>> {
        // `async move` takes ownership of `request`, so the future does not
        // borrow anything the caller might drop; it only borrows `self`.
        Box::pin(async move {
            let prompt = intent_prompt(&request);
            let value = self
                .run(INTENT_SYSTEM_PROMPT, &prompt, INTENT_SCHEMA)
                .await?;
            actions_from_json(value)
        })
    }
}

/// Print mode with JSON output checked against `schema`, every built-in
/// tool, MCP server, skill and saved session turned off.
fn cli_args(system: &str, schema: &str) -> Vec<String> {
    [
        "-p",
        "--output-format",
        "json",
        "--json-schema",
        schema,
        "--system-prompt",
        system,
        "--tools",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--no-session-persistence",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect()
}

/// The fields we read from `--output-format json`.
#[derive(Deserialize)]
struct CliResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: Option<String>,
    /// The model's text answer.
    #[serde(default)]
    result: Option<String>,
    /// The answer as JSON, when `--json-schema` was given.
    #[serde(default)]
    structured_output: Option<serde_json::Value>,
}

fn structured_output(stdout: &str) -> Result<serde_json::Value, LlmError> {
    let parsed: CliResult = serde_json::from_str(stdout.trim())
        .map_err(|err| LlmError::BadOutput(format!("CLI output is not its JSON result: {err}")))?;
    if parsed.is_error {
        let message = parsed
            .result
            .filter(|r| !r.trim().is_empty())
            .or(parsed.subtype)
            .unwrap_or_else(|| "unknown error".into());
        return Err(LlmError::Failed(message));
    }
    if let Some(value) = parsed.structured_output {
        return Ok(value);
    }
    // Older CLI versions put the JSON answer in `result` as text.
    match parsed.result.as_deref().map(str::trim) {
        None | Some("") => Err(LlmError::EmptyOutput),
        Some(text) => {
            serde_json::from_str(text).map_err(|err| LlmError::BadOutput(err.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_turn_every_tool_off() {
        let args = cli_args("system", "{}");
        let after = |flag: &str| {
            let i = args.iter().position(|a| a == flag).unwrap();
            args[i + 1].as_str()
        };
        assert_eq!(args[0], "-p");
        assert_eq!(after("--output-format"), "json");
        assert_eq!(after("--json-schema"), "{}");
        assert_eq!(after("--system-prompt"), "system");
        assert_eq!(after("--tools"), "");
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
    }

    #[test]
    fn reads_structured_output() {
        let stdout = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "",
            "structured_output": { "actions": [] }
        })
        .to_string();
        assert_eq!(
            structured_output(&stdout).unwrap(),
            json!({ "actions": [] })
        );
    }

    #[test]
    fn falls_back_to_json_in_result_text() {
        let stdout = json!({ "is_error": false, "result": "{\"actions\": []}" }).to_string();
        assert_eq!(
            structured_output(&stdout).unwrap(),
            json!({ "actions": [] })
        );
    }

    #[test]
    fn reports_cli_errors() {
        let stdout = json!({
            "is_error": true,
            "subtype": "success",
            "result": "Invalid API key · Please run /login"
        })
        .to_string();
        assert!(matches!(
            structured_output(&stdout),
            Err(LlmError::Failed(m)) if m.contains("/login")
        ));
    }

    #[test]
    fn empty_answer_is_an_error() {
        let stdout = json!({ "is_error": false, "result": "" }).to_string();
        assert!(matches!(
            structured_output(&stdout),
            Err(LlmError::EmptyOutput)
        ));
        assert!(matches!(
            structured_output("not json"),
            Err(LlmError::BadOutput(_))
        ));
    }

    /// Runs the real process plumbing against a shell script standing in for
    /// `claude`, so no test ever calls the model.
    #[cfg(unix)]
    mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        struct Fake {
            dir: PathBuf,
            cli: ClaudeCli,
        }

        impl Drop for Fake {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        fn fake(name: &str, body: &str, timeout: Duration) -> Fake {
            let dir = std::env::temp_dir()
                .join(format!("expense-llm-test-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("claude");
            std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let cli = ClaudeCli::with_program(script, dir.join("work"), timeout);
            Fake { dir, cli }
        }

        fn request(message: &str) -> IntentRequest {
            IntentRequest {
                message: message.into(),
                cards: vec![],
            }
        }

        #[tokio::test]
        async fn parses_actions_from_the_cli() {
            let answer = r#"{"is_error":false,"structured_output":{"actions":[{"type":"reply","text":"halo"}]}}"#;
            let fake = fake(
                "ok",
                &format!(
                    "cat > \"$(dirname \"$0\")/stdin\"\n\
                     env > \"$(dirname \"$0\")/env\"\n\
                     echo '{answer}'"
                ),
                Duration::from_secs(10),
            );
            let actions = fake.cli.parse(request("hai")).await.unwrap();
            assert_eq!(
                actions,
                vec![ProposedAction::Reply {
                    text: "halo".into()
                }]
            );

            let stdin = std::fs::read_to_string(fake.dir.join("stdin")).unwrap();
            assert!(stdin.contains("<message>\nhai\n</message>"), "{stdin}");
            let env = std::fs::read_to_string(fake.dir.join("env")).unwrap();
            assert!(env.contains("CLAUDE_CODE_OAUTH_TOKEN=test-token"), "{env}");
            assert!(env.contains("CLAUDE_CONFIG_DIR=/accounts/work"), "{env}");
            // Cargo sets this for test runs; the CLI must not inherit it.
            if std::env::var_os("CARGO_MANIFEST_DIR").is_some() {
                assert!(!env.contains("CARGO_MANIFEST_DIR"), "{env}");
            }
        }

        #[tokio::test]
        async fn reports_exit_without_output() {
            let fake = fake(
                "exit",
                "cat > /dev/null\necho 'boom' >&2\nexit 3",
                Duration::from_secs(10),
            );
            let result = fake.cli.parse(request("hai")).await;
            assert!(
                matches!(&result, Err(LlmError::Exit { stderr, .. }) if stderr == "boom"),
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn times_out() {
            let fake = fake("slow", "sleep 5", Duration::from_millis(200));
            let result = fake.cli.parse(request("hai")).await;
            assert!(matches!(result, Err(LlmError::Timeout(_))), "{result:?}");
        }

        #[tokio::test]
        async fn missing_program_is_a_spawn_error() {
            let fake = fake("missing", "", Duration::from_secs(10));
            let cli = ClaudeCli::with_program(
                fake.dir.join("no-such-claude"),
                fake.dir.join("work"),
                Duration::from_secs(10),
            );
            let result = cli.parse(request("hai")).await;
            assert!(
                matches!(&result, Err(LlmError::Spawn { program, .. }) if program.ends_with("no-such-claude")),
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn check_returns_the_version() {
            let fake = fake(
                "version",
                "[ \"$1\" = --version ] && echo '2.1.287 (Claude Code)'",
                Duration::from_secs(10),
            );
            assert_eq!(fake.cli.check().await.unwrap(), "2.1.287 (Claude Code)");
        }

        #[tokio::test]
        async fn check_reports_a_broken_cli() {
            let fake = fake(
                "broken",
                "echo 'no node' >&2\nexit 1",
                Duration::from_secs(10),
            );
            let result = fake.cli.check().await;
            assert!(
                matches!(&result, Err(LlmError::Exit { stderr, .. }) if stderr == "no node"),
                "{result:?}"
            );
        }
    }

    /// Calls the real `claude` CLI, on your subscription. Skipped by
    /// `cargo test`; run it on purpose with:
    ///
    /// ```sh
    /// CLAUDE_CONFIG_DIR=/path/to/account cargo test real_cli -- --ignored --nocapture
    /// ```
    ///
    /// The model is not deterministic, so a failure here means "look at the
    /// printed actions", not necessarily a bug.
    #[tokio::test]
    #[ignore = "calls the real claude CLI; run with --ignored"]
    async fn real_cli_smoke_test() {
        use crate::config::{LlmConfig, LlmMode};

        let config = LlmConfig {
            mode: LlmMode::Cli,
            oauth_token: std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok(),
            config_dir: std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        };
        let cli = ClaudeCli::new(&config);
        println!("version: {}", cli.check().await.unwrap());

        let ask = |message: &str| {
            let request = IntentRequest {
                message: message.into(),
                cards: vec![],
            };
            cli.parse(request)
        };

        let actions = ask("add my CIMB Niaga card, it ends in 4419")
            .await
            .unwrap();
        println!("register: {actions:?}");
        assert!(
            actions.iter().any(|a| matches!(a,
                ProposedAction::RegisterCard { senders, card_ref: Some(digits), .. }
                    if senders.iter().any(|s| s.contains("cimbniaga")) && digits.ends_with("19"))),
            "{actions:?}"
        );

        let actions = ask("halo!").await.unwrap();
        println!("greeting: {actions:?}");
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, ProposedAction::Reply { .. })),
            "{actions:?}"
        );

        for off_topic in [
            "what's the capital of France?",
            "write me a python script that sorts a list",
        ] {
            let actions = ask(off_topic).await.unwrap();
            println!("{off_topic:?}: {actions:?}");
            assert_eq!(actions, vec![ProposedAction::OutOfScope], "{off_topic:?}");
        }
    }
}
