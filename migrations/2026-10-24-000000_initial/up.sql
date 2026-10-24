-- Initial schema: the eight tables from docs/PLAN.md "Data model".
--
-- Conventions (the declared type decides the Rust type Diesel uses):
--   * Money in rupiah is BIGINT -> i64. Never REAL.
--   * Telegram IDs are BIGINT -> i64 (chat IDs do not fit in i32).
--   * Row IDs are INTEGER PRIMARY KEY -> i32 (SQLite's rowid alias).
--   * Timestamps are TIMESTAMP -> chrono::NaiveDateTime. Always UTC (the type
--     carries no zone, so this is a rule, not a guarantee). Shown in Asia/Jakarta.
--   * Calendar dates are DATE -> chrono::NaiveDate.
--   * Book months are TEXT 'YYYY-MM'.
--   * Booleans are BOOLEAN -> bool (stored as 0/1).

-- One row per card billing account; cards that share one bill share one row.
CREATE TABLE cards (
    id             INTEGER     PRIMARY KEY NOT NULL,
    bank           TEXT        NOT NULL,
    -- JSON array of sender addresses, e.g. '["notif@bank.co.id"]'.
    notify_senders TEXT        NOT NULL CHECK (json_valid(notify_senders)),
    -- The card digits the email shows after the mask: "7078" from
    -- "XXXX7078", "19" from CIMB's "5192-99XX-XXXX-XX19". Senders like Livin'
    -- send the same email for every card, so this picks the right one. NULL
    -- for banks whose emails show no digits.
    card_ref       TEXT        CHECK (length(card_ref) BETWEEN 2 AND 4
                                      AND card_ref NOT GLOB '*[^0-9]*'),
    -- Set with /cards in Phase 3; unknown until then.
    cutoff_day     INTEGER     CHECK (cutoff_day BETWEEN 1 AND 31),
    due_day        INTEGER     CHECK (due_day BETWEEN 1 AND 31),
    -- Running balance. Floors at zero: overpayments are dropped.
    payable_idr    BIGINT      NOT NULL DEFAULT 0 CHECK (payable_idr >= 0),
    created_at     TIMESTAMP   NOT NULL,

    -- One email must match at most one card. NULLs don't collide in SQLite,
    -- so several digit-less cards can share a bank.
    UNIQUE (bank, card_ref)
);

-- One row per notified charge. `status` drives every flow.
CREATE TABLE charges (
    id                  INTEGER     PRIMARY KEY NOT NULL,
    -- UNIQUE makes re-polling Gmail idempotent.
    gmail_msg_id        TEXT        NOT NULL UNIQUE,
    card_id             INTEGER     NOT NULL REFERENCES cards (id),
    -- Descriptor exactly as the bank wrote it.
    merchant_raw        TEXT        NOT NULL,
    -- Real merchant, when a merchant rule asked for it.
    merchant            TEXT,
    -- NULL only while a foreign charge waits for its final rupiah amount.
    amount_idr          BIGINT      CHECK (amount_idr > 0),
    -- Foreign charges: ISO currency code and the amount as written in the
    -- email (decimal text, e.g. '12.34'). Display only, never used for math.
    orig_currency       TEXT,
    orig_amount         TEXT,
    charged_at          TIMESTAMP   NOT NULL,
    book_month          TEXT,
    status              TEXT        NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'needs_amount', 'held', 'recorded')),
    needs_merchant      BOOLEAN     NOT NULL DEFAULT 0 CHECK (needs_merchant IN (0, 1)),
    -- How the email was read: a deterministic parser, or the model's quotes
    -- verified by code. Review marks 'model' charges "read by AI".
    extracted_by        TEXT        NOT NULL DEFAULT 'parser'
                        CHECK (extracted_by IN ('parser', 'model')),
    payee               TEXT,
    note                TEXT,
    -- JSON array of Lunch Money transaction IDs, one per part.
    lm_ids              TEXT        CHECK (lm_ids IS NULL OR json_valid(lm_ids)),
    telegram_message_id BIGINT,
    recorded_at         TIMESTAMP  ,
    created_at          TIMESTAMP   NOT NULL,

    -- Every status except needs_amount has a rupiah amount.
    CHECK (status = 'needs_amount' OR amount_idr IS NOT NULL),
    -- A foreign charge stores what it was charged in.
    CHECK (status <> 'needs_amount' OR (orig_currency IS NOT NULL AND orig_amount IS NOT NULL)),
    -- A held charge knows which month it waits for.
    CHECK (status <> 'held' OR book_month IS NOT NULL),
    -- A recorded charge knows when it was recorded.
    CHECK (status <> 'recorded' OR recorded_at IS NOT NULL)
);

CREATE INDEX charges_status ON charges (status, charged_at);
CREATE INDEX charges_card ON charges (card_id, charged_at);

-- Card payments you report ("paid BCA 2.5jt").
CREATE TABLE payments (
    id         INTEGER     PRIMARY KEY NOT NULL,
    card_id    INTEGER     NOT NULL REFERENCES cards (id),
    amount_idr BIGINT      NOT NULL CHECK (amount_idr > 0),
    paid_at    TIMESTAMP   NOT NULL
);

CREATE INDEX payments_card ON payments (card_id, paid_at);

-- Past answers per merchant rule, offered as buttons next time.
CREATE TABLE merchant_answers (
    id           INTEGER     PRIMARY KEY NOT NULL,
    rule_name    TEXT        NOT NULL,
    answer       TEXT        NOT NULL,
    last_used_at TIMESTAMP   NOT NULL,
    UNIQUE (rule_name, answer)
);

-- Stops duplicate due-date reminders.
CREATE TABLE reminders_sent (
    card_id     INTEGER     NOT NULL REFERENCES cards (id),
    -- Statement cycle end date.
    cycle_end   DATE        NOT NULL,
    days_before INTEGER     NOT NULL CHECK (days_before >= 0),
    sent_at     TIMESTAMP   NOT NULL,
    PRIMARY KEY (card_id, cycle_end, days_before)
);

-- Recent conversation turns, for natural follow-ups.
CREATE TABLE messages (
    id        INTEGER     PRIMARY KEY NOT NULL,
    chat_id   BIGINT      NOT NULL,
    role      TEXT        NOT NULL CHECK (role IN ('user', 'assistant')),
    text      TEXT        NOT NULL,
    -- Cleanup deletes old charges; keep the turn, drop the link.
    charge_id INTEGER     REFERENCES charges (id) ON DELETE SET NULL,
    sent_at   TIMESTAMP   NOT NULL
);

CREATE INDEX messages_chat ON messages (chat_id, sent_at);

-- Open questions the bot is waiting on.
CREATE TABLE pending_inputs (
    id         INTEGER     PRIMARY KEY NOT NULL,
    chat_id    BIGINT      NOT NULL,
    -- A question about a deleted charge is meaningless; drop it too.
    charge_id  INTEGER     REFERENCES charges (id) ON DELETE CASCADE,
    kind       TEXT        NOT NULL,
    question   TEXT        NOT NULL,
    expires_at TIMESTAMP
);

CREATE INDEX pending_inputs_chat ON pending_inputs (chat_id);

-- The merchant rules document, one row per saved version. Newest is live.
CREATE TABLE merchant_docs (
    version  INTEGER     PRIMARY KEY NOT NULL,
    content  TEXT        NOT NULL,
    saved_at TIMESTAMP   NOT NULL
);
