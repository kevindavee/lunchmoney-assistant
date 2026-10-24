//! Mandiri Kartu Kredit transaction emails, sent for card payments at a
//! terminal or online, from `mandiricare.onlinenotif@bankmandiri.co.id`.
//!
//! The email names the card by its last four digits ("4 digit akhir kartu
//! XXXX1234") and lists "Label : value" lines for the time, merchant and
//! amount.

use chrono::{FixedOffset, NaiveDateTime};

use super::{ParseError, ParsedCharge, Tokens, parse_rupiah};

/// Name of this template, used to pick the parser for an email.
pub const NAME: &str = "mandiri_card";

/// Where these emails come from.
pub const SENDER: &str = "mandiricare.onlinenotif@bankmandiri.co.id";

/// The email gives no time zone. Mandiri is a Jakarta bank and its other
/// emails say WIB, so we read these times as WIB (UTC+7).
const WIB_SECONDS: i32 = 7 * 3600;

/// Parses the plain-text body of the email.
pub fn parse(body: &str) -> Result<ParsedCharge, ParseError> {
    let tokens = Tokens::new(body);

    let card_ref = parse_card_digits(body)?;

    let raw_time = tokens.value("Tanggal/waktu")?;
    let local = NaiveDateTime::parse_from_str(raw_time, "%d-%m-%Y %H:%M:%S").map_err(|_| {
        ParseError::BadValue {
            field: "Tanggal/waktu",
            value: raw_time.to_string(),
        }
    })?;
    let wib = FixedOffset::east_opt(WIB_SECONDS).expect("UTC+7 is a valid offset");

    let merchant_raw = tokens.value("Merchant/ATM")?.to_string();
    let amount_idr = parse_rupiah("Sejumlah", tokens.value("Sejumlah")?)?;

    Ok(ParsedCharge {
        merchant_raw,
        amount_idr,
        charged_at: super::to_utc(local.date(), local.time(), wib),
        card_ref,
        card_name: None,
    })
}

/// Finds "4 digit akhir kartu XXXX1234" and returns "1234".
fn parse_card_digits(body: &str) -> Result<String, ParseError> {
    const LABEL: &str = "4 digit akhir kartu";
    let after = body
        .split_once(LABEL)
        .ok_or(ParseError::MissingField(LABEL))?
        .1;
    let masked = after.split_whitespace().next().unwrap_or_default();
    let digits = masked.trim_start_matches(['X', 'x', '*']);
    let valid = masked.len() > digits.len()
        && digits.len() == 4
        && digits.chars().all(|c| c.is_ascii_digit());
    if valid {
        Ok(digits.to_string())
    } else {
        Err(ParseError::BadValue {
            field: LABEL,
            value: masked.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHARGE: &str = include_str!("../../tests/fixtures/mandiri_card/charge.txt");

    #[test]
    fn parses_card_transaction() {
        let charge = parse(CHARGE).unwrap();
        assert_eq!(charge.merchant_raw, "PHYSIOWORKS-HO TANGE");
        assert_eq!(charge.amount_idr, 420_000);
        // 18:37:05 WIB is 11:37:05 UTC.
        assert_eq!(charge.charged_at.to_string(), "2026-10-21 11:37:05");
        assert_eq!(charge.card_ref, "1234");
        assert_eq!(charge.card_name, None);
    }

    #[test]
    fn missing_amount_is_reported() {
        let body = CHARGE.replace("Sejumlah", "Total");
        assert_eq!(parse(&body), Err(ParseError::MissingField("Sejumlah")));
    }

    #[test]
    fn changed_date_format_is_refused() {
        let body = CHARGE.replace("21-10-2026 18:37:05", "2026-10-21 18:37");
        assert!(matches!(
            parse(&body),
            Err(ParseError::BadValue {
                field: "Tanggal/waktu",
                ..
            })
        ));
    }

    #[test]
    fn foreign_currency_is_refused_for_now() {
        // Foreign charges get their own flow in Phase 2; until then, alert.
        let body = CHARGE.replace("IDR420.000,00", "USD25,00");
        assert!(parse(&body).is_err());
    }

    #[test]
    fn card_digits_must_be_masked_and_four_long() {
        assert_eq!(
            parse_card_digits("4 digit akhir kartu XXXX1234"),
            Ok("1234".into())
        );
        assert!(parse_card_digits("4 digit akhir kartu 1234").is_err());
        assert!(parse_card_digits("4 digit akhir kartu XXXX123").is_err());
        assert!(parse_card_digits("no digits here").is_err());
    }
}
