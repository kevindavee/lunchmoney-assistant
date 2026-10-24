// @generated automatically by Diesel CLI.

diesel::table! {
    cards (id) {
        id -> Integer,
        bank -> Text,
        notify_senders -> Text,
        card_ref -> Nullable<Text>,
        cutoff_day -> Nullable<Integer>,
        due_day -> Nullable<Integer>,
        payable_idr -> BigInt,
        created_at -> Timestamp,
    }
}

diesel::table! {
    charges (id) {
        id -> Integer,
        gmail_msg_id -> Text,
        card_id -> Integer,
        merchant_raw -> Text,
        merchant -> Nullable<Text>,
        amount_idr -> Nullable<BigInt>,
        orig_currency -> Nullable<Text>,
        orig_amount -> Nullable<Text>,
        charged_at -> Timestamp,
        book_month -> Nullable<Text>,
        status -> Text,
        needs_merchant -> Bool,
        extracted_by -> Text,
        payee -> Nullable<Text>,
        note -> Nullable<Text>,
        lm_ids -> Nullable<Text>,
        telegram_message_id -> Nullable<BigInt>,
        recorded_at -> Nullable<Timestamp>,
        created_at -> Timestamp,
    }
}

diesel::table! {
    merchant_answers (id) {
        id -> Integer,
        rule_name -> Text,
        answer -> Text,
        last_used_at -> Timestamp,
    }
}

diesel::table! {
    merchant_docs (version) {
        version -> Integer,
        content -> Text,
        saved_at -> Timestamp,
    }
}

diesel::table! {
    messages (id) {
        id -> Integer,
        chat_id -> BigInt,
        role -> Text,
        text -> Text,
        charge_id -> Nullable<Integer>,
        sent_at -> Timestamp,
    }
}

diesel::table! {
    payments (id) {
        id -> Integer,
        card_id -> Integer,
        amount_idr -> BigInt,
        paid_at -> Timestamp,
    }
}

diesel::table! {
    pending_inputs (id) {
        id -> Integer,
        chat_id -> BigInt,
        charge_id -> Nullable<Integer>,
        kind -> Text,
        question -> Text,
        expires_at -> Nullable<Timestamp>,
    }
}

diesel::table! {
    reminders_sent (card_id, cycle_end, days_before) {
        card_id -> Integer,
        cycle_end -> Date,
        days_before -> Integer,
        sent_at -> Timestamp,
    }
}

diesel::joinable!(charges -> cards (card_id));
diesel::joinable!(messages -> charges (charge_id));
diesel::joinable!(payments -> cards (card_id));
diesel::joinable!(pending_inputs -> charges (charge_id));
diesel::joinable!(reminders_sent -> cards (card_id));

diesel::allow_tables_to_appear_in_same_query!(
    cards,
    charges,
    merchant_answers,
    merchant_docs,
    messages,
    payments,
    pending_inputs,
    reminders_sent,
);
