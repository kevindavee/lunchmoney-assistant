# Expense Assistant — Build Plan

Last updated: Oct 18, 2026

## Goal and scope

A personal Telegram bot that turns credit-card notification emails into Lunch Money entries, tracks what you owe each card, and reminds you before due dates. You review once a day; nothing reaches Lunch Money without your confirmation.

**In scope**

- Ingest card notifications from Gmail automatically
- Daily review in Telegram: record, split, record partial, hold
- Ask for the real merchant behind e-wallet charges, the venue behind padel bookings, and the final amount for foreign-currency charges
- Categorize with Lunch Money rules first, then Jev, then ask you
- Track payable per card and log the card payments you report
- Remind you before each card's due date, from notified charges only
- Trash the email once its charge is recorded

**Out of scope**

- Ingesting or reconciling bank statements (separate topic)
- Charges that never produced a notification
- Recording card payments in Lunch Money (expenses are declared individually, as if paid by debit)
- Tracking who owes you after a split

## Locked decisions

Everything below was settled in discussion; revisit only with new facts.

| Area | Decision | Why |
| --- | --- | --- |
| Channel | Telegram bot: Bot API, webhook, inline buttons | Free and official; the bot can message you first. WhatsApp needs paid templates or an unofficial bridge with ban risk |
| Language | Rust | Learning goal; one small binary |
| Database | SQLite on a persistent volume | Transactions and `UNIQUE` constraints; free |
| Hosting | Fly.io, one always-on machine (512MB to 1GB), Singapore region, auto-stop off | Needs a disk and the `claude` CLI; timers must keep firing |
| Email | Gmail API, OAuth app set to In production, scope `gmail.modify`, poll every 5 to 10 min | Narrow, revocable access; polling cannot lapse silently like a 7-day watch |
| Books | Lunch Money v2 API, posting to the Monthly Budget account, inserts with `apply_rules` on | The rules flag did not work in v1 |
| LLM | Claude Pro via `claude -p --output-format json --json-schema`, built-in tools disabled, behind an `IntentParser` trait | Uses the existing subscription; switch to an API key with one config flag |
| Categorization | Lunch Money rules, then Jev, then ask you | Rules cost nothing and never guess |
| Money | Rupiah stored as `i64` | No floating-point errors |
| Retention | Delete rows older than 3 months once recorded | Short-lived working data, not an archive |

## Architecture

One Rust process on one Fly machine. Every outside service is called from it, and none of them holds state you depend on.

```
Gmail (poll 5-10 min) <-> Ingestor ----------+
You on Telegram       <-> Bot handlers ------+--> Ledger core --> SQLite (Fly volume)
                          Scheduler ---------+        |
                                                      +--> Lunch Money API
Bot handlers <-> claude -p (Pro): text -> JSON proposal
Ledger core  ->  Jev API: category guess
```

The ingestor, bot handlers and scheduler all go through the ledger core, and only the core writes to SQLite. The model only turns your free text into a proposal; it never calls Gmail, Lunch Money or the database.

## Data model

Eight small tables. `charges.status` drives every flow, and only a recorded charge changes what you owe the card.

| Table | Holds | Key columns |
| --- | --- | --- |
| `cards` | One row per card billing account; cards that share one bill share one row | bank, notify\_senders, subject\_match, parser, cutoff\_day, due\_day, payable\_idr |
| `charges` | One row per notified charge | gmail\_msg\_id (`UNIQUE`), card\_id, merchant\_raw, merchant, amount\_idr, orig\_currency, orig\_amount, charged\_at, book\_month, status, needs\_merchant, payee, note, lm\_ids, telegram\_message\_id, recorded\_at |
| `payments` | Card payments you report | card\_id, amount\_idr, paid\_at |
| `merchant_answers` | Past answers per merchant rule, offered as buttons next time | rule\_name, answer, last\_used\_at |
| `reminders_sent` | Stops duplicate due-date reminders | card\_id, cycle\_end, days\_before |
| `messages` | Recent conversation turns, for natural follow-ups | chat\_id, role, text, charge\_id, sent\_at |
| `pending_inputs` | Open questions the bot is waiting on | chat\_id, charge\_id, kind, question, expires\_at |
| `merchant_docs` | The merchant rules document, one row per saved version | version, content, saved\_at |

**Merchant rules live in one markdown document stored in SQLite** (`merchant_docs`, one row per version; the newest is live). Code reads only the `match` and `ask` lines, to flag charges at ingestion. The model gets the whole document as context for parsing and categorization. Remembered answers are written by code to `merchant_answers`, never back into the document.

```markdown
## AYO
match: "AYO*"
ask: Which venue?
---
Padel booking platform. Use the venue as payee, never "AYO".
Category: Sports.
```

```
Ingested --IDR--> Pending --confirm--> Recorded (adds to payable, email trashed)
   |                 ^  ^
   +--foreign--> Needs final amount --amount sent--+
                    |
                    +--hold/release--> Held (next book month)
```

A foreign charge waits for its final rupiah amount before review. A merchant-rule question is a flag on a pending charge, not a separate state. Held charges go back to pending when their book month arrives or you release them.

## Core flows and rules

Every write follows one path: the model proposes, the code validates, you confirm, then Lunch Money and the database are updated.

### Ingest

- Poll Gmail every 5 to 10 minutes, only for notification senders registered with /cards.
- Parse each bank's template with deterministic code. The model never reads amounts.
- Insert the charge with a `UNIQUE` Gmail message ID, then label the email `expense/ingested`.
- Non-rupiah amount: status `needs_amount`, with the original currency and amount stored.
- Descriptor matches a merchant rule (e-wallet, payment gateway, padel booking platform): flag `needs_merchant`.
- Unparseable email: send a Telegram alert with the subject line. Never guess. A registered sender whose email matches no card gets an alert with a Register button.

### Daily review

- Runs at 20:00 WIB, and on demand with `/review`.
- One message per pending charge, oldest first, each with its own buttons.
- Foreign charges ask for the final rupiah amount and whether it is ready to record.
- Charges flagged by a merchant rule ask that rule's question (real merchant, padel venue) and offer past answers as buttons. The answer becomes the payee, so Lunch Money gets the venue, not the booking platform.
- Held charges appear as one summary line: count and total.

### Record actions

| Action | Lunch Money gets | Payable gets | Email |
| --- | --- | --- | --- |
| Record | One transaction, full amount | Full card amount | Trashed |
| Split | Several parts that sum to the card amount, a category each | Full card amount | Trashed |
| Partial | Your share only | Full card amount | Trashed |
| Hold | Nothing yet; a book month is chosen | Nothing until recorded | Stays in inbox |

- Each Lunch Money write carries `external_id` = charge ID plus part number, so a retry after a crash cannot duplicate it.
- Order of writes: Lunch Money, then the database, then trash the email. A failure at any step leaves the charge pending and alerts you.

**Payee and note.** Any record can carry an optional payee and note. The payee defaults to the merchant from the email; you can name another one or leave it blank. A blank payee means Lunch Money rules cannot match, so the category comes from Jev or from you.

### Categorization

1. Insert into Lunch Money with `apply_rules`, using the real merchant as payee.
2. Read the transaction back. A category is set: keep it.
3. No category: send Jev the merchant, amount and your category list.
4. Confidence of 85% or more: write it and reply "Recorded as Groceries \[Change\]".
5. Below 85%: ask you with category buttons. The transaction stays uncategorized until you answer.

### Payable and payments

- Each card keeps a running `payable_idr` balance, updated in the same database transaction as the event.
- Recording a charge adds its full card amount. "Paid BCA 2.5jt" subtracts 2,500,000.
- No matching of payments to individual charges.
- Overpaying floors the balance at zero; any excess is dropped.
- `/balances` lists payable per card, plus the held total separately.

### Due-date reminder

- Each card stores its statement cutoff day and due day.
- Estimate = all notified charges dated in the last closed cycle (any status, held included) minus payments made since that cutoff.
- If above zero, remind at 09:00 WIB, 5 days and 2 days before the due date, and on the day.
- The message is labeled "notified charges only", because unnotified charges are missing from it.

### Cleanup

- Runs monthly.
- Deletes charges recorded more than 3 months ago, and payments older than 3 months.
- Keeps pending, held and needs-input charges, card settings, the running balances, the merchant rules document and its answers.

## Telegram UX

Buttons handle the common path; for anything else, talk to it in your own words. The bot answers only your Telegram user ID.

**One review message per charge**

```
🟡 BCA •4417 — Rp 1.240.000
SUSHI TEI PIK · Thu 16 Oct 20:14
[✅ Record] [✂️ Split] [➗ Partial] [⏸ Hold]
```

**Examples of what you might say** (no fixed phrasing; every result is shown back for you to confirm)

| You might say | Becomes |
| --- | --- |
| split 310k dining, rest treat | Split: 310,000 Dining, 930,000 Treat |
| I only owe 150k of this, friends paid the rest | Partial: 150,000 recorded, full amount to payable |
| record as Hokben, note team lunch | Record with payee Hokben, note "team lunch" |
| no payee, just note it as parking | Record with no payee, note "parking" |
| that was Indomaret | Real merchant for an e-wallet charge |
| padel was at Court 9 Kemang | Payee set to the venue, not the booking platform |
| it came to 1.234.567 in the end | Final rupiah amount for a foreign charge |
| keep this one for next month | Hold, book month set to next month |
| the Gopay one was Indomaret, split the sushi 310k dining rest treat, hold the shoes | Three actions on three charges, confirmed together |
| paid BCA 2.5jt | Payment of 2,500,000 on the BCA card |

**Conversation memory.** Each message is still one stateless model call. Code adds the charge you replied to (or the one awaiting input), your categories, and the last 10 turns from `messages`, so "no, make the treat 400k" works. The window resets after 30 minutes idle, after a charge is recorded, or on /new. One message can answer several open questions: the model returns one action per charge, each must match an open question, anything unmatched stays open, and you confirm the whole set.

**Commands**

- `/review`: run the daily review now
- `/balances`: payable per card and the held total
- `/held`: list held charges with their book months
- `/cards`: register a card by its notification email (sender and subject) and set its cutoff and due days; a new bank also needs its parser written in code
- `/merchants`: get the merchant rules document and remembered answers; send back an edited copy to save a new version, after validation and a diff
- `/new`: clear the conversation window
- `/help`: example phrases

## Build phases

Ship the happy path for one card first; each phase ends with a check you can run against real emails.

### Phase 0: accounts and plumbing

- [ ] Create the Telegram bot with BotFather; note your own user ID for the allowlist
- [ ] Google Cloud project, Gmail API on, OAuth consent set to In production, refresh token for `gmail.modify`
- [ ] Lunch Money v2 API token; note the Monthly Budget account ID
- [ ] `claude setup-token` for the long-lived subscription token
- [ ] Fly app in Singapore, 1GB volume, secrets set, auto-stop off
- [ ] Docker image: Rust binary plus Node plus the `claude` CLI

**Done when:** the bot replies "pong" from Fly and can list your 5 newest Gmail subjects.

### Phase 1: one card, happy path

- [ ] SQLite schema and migrations
- [ ] Card registration, Gmail poller and a parser for one bank, written from a sample notification email
- [ ] Daily review with the Record button only
- [ ] Lunch Money insert with `apply_rules` and `external_id`
- [ ] Trash the email after recording
- [ ] `/paid` free text and `/balances`
- [ ] Weekly backup: the bot sends you the SQLite file

**Done when:** a real charge goes from email to Lunch Money in one review, its email is trashed, the balance is right, and re-running the poller adds no duplicate.

### Phase 2: the edge cases

- [ ] `IntentParser` trait with the `claude -p` implementation and a JSON schema that returns a list of actions
- [ ] Split, Partial and Hold, with validation that parts sum correctly
- [ ] Foreign-currency flow (`needs_amount`)
- [ ] Versioned merchant rules document and merchant-rule flow (e-wallet, padel venue) (`needs_merchant`)
- [ ] Jev categorization with ask-on-unsure, and the rolling conversation window

**Done when:** each edge case has run once on a real charge without a manual fix in Lunch Money.

### Phase 3: reminders and upkeep

- [ ] `/cards` for cutoff and due days
- [ ] Due-date reminders
- [ ] Monthly cleanup
- [ ] Parsers for the remaining banks
- [ ] Failure alerts (see Operations)

**Done when:** one full statement cycle passes with reminders sent and no missed alerts.

## Operations

Any failure must reach you on Telegram; a bot that stops quietly is the main failure mode.

**Secrets** (Fly secrets, never in the image or repo)

- Telegram bot token, plus a webhook secret checked on every request
- Your Telegram user ID for the allowlist
- Google OAuth client ID, client secret, refresh token
- Lunch Money API token
- Claude subscription token from `claude setup-token`
- Jev API key

**Alerts to Telegram**

| Signal | Likely cause |
| --- | --- |
| Gmail `invalid_grant` | Refresh token revoked, unused 6 months, or password changed: redo OAuth |
| No successful poll in 60 minutes | Poller crashed or Gmail is down |
| `claude -p` auth error or empty output | Subscription token expired or policy changed: switch `LLM_MODE` |
| Parser could not read a bank email | Bank changed its template |
| Lunch Money write failed | API error; the charge stays pending |

**Backups**

- Fly volume snapshots
- A weekly `.backup` copy of the SQLite file sent to you as a Telegram document

## Risks

The biggest risk is the subscription route changing under you; the `IntentParser` trait makes that a config change.

| Risk | Effect | Fallback |
| --- | --- | --- |
| Anthropic changes subscription use for the Agent SDK or `claude -p` again | Free-text parsing stops | Set `LLM_MODE=api` with a pay-as-you-go key, about $1 to 2 a month |
| A bank changes its email template | Charges from that bank are missed | Parser-failure alert; fix that bank's parser |
| Missing notifications | Due-date reminders understate the bill | Accepted; the reminder is labeled an estimate |
| Gmail token revoked | Ingestion stops | `invalid_grant` alert; redo OAuth |
| Fly volume tied to one machine | Data loss if the volume fails | Snapshots plus the weekly backup file |
