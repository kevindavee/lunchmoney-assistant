# AGENTS.md

Personal Telegram bot that turns credit-card notification emails into Lunch Money entries.
The full design is in `docs/PLAN.md`. Read it before any task; it is the source of truth.
If a request conflicts with the plan, stop and ask instead of improvising.

The owner is learning Rust. Prefer clear, idiomatic code over clever code, and explain
non-obvious Rust (lifetimes, async, trait objects) in a short comment or in your reply.

## Work in phases

Build only the current phase from `docs/PLAN.md` ("Build phases"). Do not start a later
phase's features early. Finish each phase with its "Done when" check.

Work one task at a time: plan briefly, implement, run `cargo fmt`, `cargo clippy -- -D warnings`
and `cargo test`, then stop and summarize what changed.

## Stack

- Rust stable, `tokio`
- `teloxide` for Telegram (webhook in production, long polling allowed locally)
- `rusqlite` (bundled) with plain SQL migrations in `migrations/`
- `reqwest` + `serde` for Gmail, Lunch Money v2 and Jev
- `yup-oauth2` (or a hand-rolled refresh-token exchange) for Gmail, scope `gmail.modify`
- `anyhow` in binaries, `thiserror` in library modules; `tracing` for logs
- Config from environment variables only; never commit secrets. Keep `.env.example` current.

## Hard rules (never break these)

1. **Money is `i64` rupiah.** No `f32`/`f64` anywhere near amounts. Parse "2.5jt", "310k",
   "1.234.567" into `i64` with unit-tested helpers.
2. **The model never reads amounts from emails.** Bank emails are parsed by deterministic
   per-bank parsers in `src/parsers/`, each with tests built from a real sample email
   stored in `tests/fixtures/` (redact anything personal except the fields being parsed).
3. **Nothing is written without confirmation.** Flow is always: model proposes ->
   code validates -> user taps confirm -> write. The model has no tools.
4. **The LLM is behind a trait.** `IntentParser` returns `Vec<ProposedAction>`. The
   default implementation shells out to `claude -p --output-format json --json-schema ...`
   with built-in tools disabled. Selected by `LLM_MODE`. No other module calls the model.
5. **Validate every proposal in code:** split parts sum to the charge amount, the category
   exists, each action targets a charge with an open question, payments name a known card.
6. **Only the ledger core writes to SQLite.** Telegram handlers, the ingestor and the
   scheduler call into it.
7. **Idempotency:** `charges.gmail_msg_id` is `UNIQUE`; every Lunch Money write carries
   `external_id` = charge ID + part number. Write order: Lunch Money -> SQLite -> trash email.
   Any failure leaves the charge pending and sends a Telegram alert.
8. **Lunch Money inserts set `apply_rules: true`** and post to the configured
   Monthly Budget account.
9. **Payable per card floors at zero.** Recording adds the full card amount (even for
   partial records); payments subtract.
10. **Allowlist:** the bot ignores every Telegram user except `TELEGRAM_OWNER_ID`, and
    checks the webhook secret header on every request.
11. **Failures are loud:** anything listed under "Alerts to Telegram" in the plan must
    actually send one.

## Layout

```
src/
  main.rs          wiring: config, db, bot, scheduler
  config.rs
  db/              migrations runner, queries
  ledger/          state machine, payable, validation (pure, heavily tested)
  parsers/         one module per bank
  gmail.rs
  lunchmoney.rs
  jev.rs
  llm/             IntentParser trait, ClaudeCli impl, JSON schema
  telegram/        handlers, keyboards, message formatting
  scheduler.rs     daily review 20:00 WIB, reminders 09:00 WIB, cleanup, backup
migrations/
tests/fixtures/
docs/PLAN.md
```

Times are stored in UTC and shown in Asia/Jakarta.

## Testing

- Unit-test the ledger, money parsing and every bank parser.
- Mock external HTTP in tests; never hit real Gmail or Lunch Money from `cargo test`.
- Never run commands that send real Telegram messages or write to Lunch Money unless
  the owner asks.

## Commits

Commit only when the owner asks. Use Conventional Commits:

```
type(scope): summary

Optional body: why this change, or the context needed to understand it.
```

- `type`: `feat`, `fix`, `refactor`, `test`, `docs`, `build`, `chore`, `perf`, `ci`.
- `scope` (optional): the area touched, e.g. `db`, `parsers`, `gmail`, `telegram`,
  `ledger`, `llm`, `plan`.
- Summary: imperative mood, lowercase, no trailing period, at most 72 characters.
- Body only when the subject is not enough, and only to explain **why** or give
  context: the reason, the constraint, the decision and what was rejected. Never
  restate what the diff already shows. Wrap at 72 characters.
- One logical change per commit; split unrelated changes. Every commit should build
  and pass `cargo test` on its own.
