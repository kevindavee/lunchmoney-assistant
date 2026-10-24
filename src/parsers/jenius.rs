//! Jenius Credit Card transaction emails, from `jenius_noreply@smbci.com`.
//!
//! English text with "Label: value" lines; the amount is in English format
//! ("IDR 20,500.00") and the date uses English month names
//! ("23 OCT 2026 19:02:08 WIB").

use super::{
    ParseError, ParsedCharge, Tokens, bad_value, masked_card_ref, parse_indonesian_time,
    parse_named_date, parse_rupiah_english, to_utc,
};

/// Name of this template, used to pick the parser for an email.
pub const NAME: &str = "jenius";

/// Where these emails come from.
pub const SENDER: &str = "jenius_noreply@smbci.com";

/// Parses the plain-text body of the email.
pub fn parse(body: &str) -> Result<ParsedCharge, ParseError> {
    let tokens = Tokens::new(body);

    let card_ref = masked_card_ref("Card number", tokens.value("Card number")?, 4)?;
    let merchant_raw = tokens.value("Merchant")?.to_string();
    let amount_idr = parse_rupiah_english("Total", tokens.value("Total")?)?;

    // "23 OCT 2026 19:02:08 WIB": three words of date, then time and zone.
    const TIME_LABEL: &str = "Transaction date & time";
    let raw_time = tokens.value(TIME_LABEL)?;
    let words: Vec<&str> = raw_time.split_whitespace().collect();
    if words.len() != 5 {
        return Err(bad_value(TIME_LABEL, raw_time));
    }
    let date = parse_named_date(TIME_LABEL, &words[..3].join(" "))?;
    let (time, offset) = parse_indonesian_time(TIME_LABEL, &words[3..].join(" "))?;

    Ok(ParsedCharge {
        merchant_raw,
        amount_idr,
        charged_at: to_utc(date, time, offset),
        card_ref,
        card_name: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHARGE: &str = include_str!("../../tests/fixtures/jenius/charge.txt");

    #[test]
    fn parses_card_transaction() {
        let charge = parse(CHARGE).unwrap();
        assert_eq!(charge.merchant_raw, "GOPAYID 1500729 ID");
        assert_eq!(charge.amount_idr, 20_500);
        // 19:02:08 WIB is 12:02:08 UTC.
        assert_eq!(charge.charged_at.to_string(), "2026-10-23 12:02:08");
        assert_eq!(charge.card_ref, "1234");
        assert_eq!(charge.card_name, None);
    }

    #[test]
    fn indonesian_amount_format_is_refused() {
        let body = CHARGE.replace("IDR 20,500.00", "IDR 20.500,00");
        assert!(matches!(
            parse(&body),
            Err(ParseError::BadValue { field: "Total", .. })
        ));
    }

    #[test]
    fn missing_time_zone_is_refused() {
        let body = CHARGE.replace("19:02:08 WIB", "19:02:08");
        assert!(matches!(
            parse(&body),
            Err(ParseError::BadValue {
                field: "Transaction date & time",
                ..
            })
        ));
    }

    #[test]
    fn missing_card_number_is_reported() {
        let body = CHARGE.replace("Card number:", "Card:");
        assert_eq!(parse(&body), Err(ParseError::MissingField("Card number")));
    }
}
