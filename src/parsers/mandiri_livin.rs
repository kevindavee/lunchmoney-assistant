//! Livin' by Mandiri "Pembayaran Berhasil!" emails: a QRIS payment made in
//! the Livin' app, from `noreply.livin@bankmandiri.co.id`.
//!
//! The email lists label/value pairs ("Nominal Transaksi  Rp 31.200,00"),
//! the merchant under "Penerima" and the card under "Sumber Dana".

use super::{
    ParseError, ParsedCharge, Tokens, masked_card_ref, parse_indonesian_time, parse_named_date,
    parse_rupiah, to_utc,
};

/// Name of this template, used to pick the parser for an email.
pub const NAME: &str = "mandiri_livin";

/// Where these emails come from. Livin' uses this sender for transfers,
/// top-ups and promos too, so the body decides what is a card charge.
pub const SENDER: &str = "noreply.livin@bankmandiri.co.id";

/// Parses the plain-text body of the email.
pub fn parse(body: &str) -> Result<ParsedCharge, ParseError> {
    let tokens = Tokens::new(body);

    // "Penerima" is followed by the merchant name, then its city.
    let merchant_raw = tokens.value("Penerima")?.to_string();

    let date = parse_named_date("Tanggal", tokens.value("Tanggal")?)?;
    let (time, offset) = parse_indonesian_time("Jam", tokens.value("Jam")?)?;
    let amount_idr = parse_rupiah("Nominal Transaksi", tokens.value("Nominal Transaksi")?)?;

    // "Sumber Dana" is followed by "Kartu Kredit - <card name>" and "****1234".
    let source = tokens.after("Sumber Dana", 2)?;
    let card_name = source[0]
        .strip_prefix("Kartu Kredit - ")
        .ok_or_else(|| ParseError::NotCreditCard(source[0].to_string()))?
        .trim()
        .to_string();
    let card_ref = masked_card_ref("Sumber Dana", source[1], 4)?;

    Ok(ParsedCharge {
        merchant_raw,
        amount_idr,
        charged_at: to_utc(date, time, offset),
        card_ref,
        card_name: Some(card_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const QRIS_CREDIT_CARD: &str =
        include_str!("../../tests/fixtures/mandiri_livin/qris_credit_card.txt");

    #[test]
    fn parses_qris_payment_by_credit_card() {
        let charge = parse(QRIS_CREDIT_CARD).unwrap();
        assert_eq!(charge.merchant_raw, "Michie Laundry");
        assert_eq!(charge.amount_idr, 31_200);
        // 13:18:34 WIB is 06:18:34 UTC.
        assert_eq!(charge.charged_at.to_string(), "2026-10-23 06:18:34");
        assert_eq!(charge.card_ref, "1234");
        assert_eq!(
            charge.card_name.as_deref(),
            Some("Marriott Bonvoy® Mandiri")
        );
    }

    #[test]
    fn non_credit_card_source_is_refused() {
        let debit = QRIS_CREDIT_CARD.replace(
            "Kartu Kredit - Marriott Bonvoy® Mandiri",
            "Tabungan - Mandiri Tabungan Rupiah",
        );
        assert_eq!(
            parse(&debit),
            Err(ParseError::NotCreditCard(
                "Tabungan - Mandiri Tabungan Rupiah".into()
            ))
        );
    }

    #[test]
    fn missing_amount_is_reported() {
        let body = QRIS_CREDIT_CARD.replace("Nominal Transaksi", "Total");
        assert_eq!(
            parse(&body),
            Err(ParseError::MissingField("Nominal Transaksi"))
        );
    }

    #[test]
    fn changed_amount_format_is_refused() {
        let body = QRIS_CREDIT_CARD.replace("Rp 31.200,00", "IDR 31,200.00");
        assert!(matches!(
            parse(&body),
            Err(ParseError::BadValue {
                field: "Nominal Transaksi",
                ..
            })
        ));
    }

    #[test]
    fn unmasked_or_short_card_number_is_refused() {
        for bad in ["1234", "****123"] {
            let body = QRIS_CREDIT_CARD.replace("****1234", bad);
            assert!(parse(&body).is_err(), "{bad:?} should be refused");
        }
    }
}
