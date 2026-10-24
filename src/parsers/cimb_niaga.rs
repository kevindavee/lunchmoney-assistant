//! CIMB Niaga credit card transaction emails, from
//! `CREDITCARD.NOTIFICATION@cimbniaga.co.id`.
//!
//! "Label : value" lines; the amount is in English format
//! ("126,400.00 IDR") and the card shows only its last two digits
//! ("5192-99XX-XXXX-XX19").

use chrono::NaiveDateTime;

use super::{
    ParseError, ParsedCharge, Tokens, bad_value, masked_card_ref, parse_rupiah_english, to_utc, wib,
};

/// Name of this template, used to pick the parser for an email.
pub const NAME: &str = "cimb_niaga";

/// Where these emails come from, lowercased for matching.
pub const SENDER: &str = "creditcard.notification@cimbniaga.co.id";

/// Parses the plain-text body of the email.
pub fn parse(body: &str) -> Result<ParsedCharge, ParseError> {
    let tokens = Tokens::new(body);

    let card_ref = masked_card_ref("No. Kartu Kredit", tokens.value("No. Kartu Kredit")?, 2)?;
    let amount_idr = parse_rupiah_english("Jumlah Transaksi", tokens.value("Jumlah Transaksi")?)?;

    const TIME_LABEL: &str = "Tanggal/Waktu Transaksi";
    let raw_time = tokens.value(TIME_LABEL)?;
    let local = NaiveDateTime::parse_from_str(raw_time, "%Y-%m-%d/%H:%M:%S")
        .map_err(|_| bad_value(TIME_LABEL, raw_time))?;

    let merchant_raw = tokens.value("Nama Merchant")?.to_string();

    Ok(ParsedCharge {
        merchant_raw,
        amount_idr,
        // The email gives no time zone; CIMB Niaga is a Jakarta bank.
        charged_at: to_utc(local.date(), local.time(), wib()),
        card_ref,
        card_name: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHARGE: &str = include_str!("../../tests/fixtures/cimb_niaga/charge.txt");

    #[test]
    fn parses_card_transaction() {
        let charge = parse(CHARGE).unwrap();
        assert_eq!(
            charge.merchant_raw,
            "Grab* A-AAAAAAAAAAAAAA JAKARTA PUSAT IDN"
        );
        assert_eq!(charge.amount_idr, 126_400);
        // 10:27:41 WIB is 03:27:41 UTC.
        assert_eq!(charge.charged_at.to_string(), "2026-10-23 03:27:41");
        assert_eq!(charge.card_ref, "34");
        assert_eq!(charge.card_name, None);
    }

    #[test]
    fn indonesian_amount_format_is_refused() {
        // If CIMB switched formats, "126.400,00" must not be read as 126.
        let body = CHARGE.replace("126,400.00 IDR", "126.400,00 IDR");
        assert!(matches!(
            parse(&body),
            Err(ParseError::BadValue {
                field: "Jumlah Transaksi",
                ..
            })
        ));
    }

    #[test]
    fn foreign_currency_is_refused_for_now() {
        let body = CHARGE.replace("126,400.00 IDR", "8.00 USD");
        assert!(parse(&body).is_err());
    }

    #[test]
    fn changed_date_format_is_refused() {
        let body = CHARGE.replace("2026-10-23/10:27:41", "23-10-2026 10:27:41");
        assert!(matches!(
            parse(&body),
            Err(ParseError::BadValue {
                field: "Tanggal/Waktu Transaksi",
                ..
            })
        ));
    }

    #[test]
    fn missing_merchant_is_reported() {
        let body = CHARGE.replace("Nama Merchant", "Merchant");
        assert_eq!(parse(&body), Err(ParseError::MissingField("Nama Merchant")));
    }
}
