//! Deterministic parsers for bank notification emails, one module per email
//! template, picked by sender. If a parser cannot read an email exactly, it
//! returns an error and the ingestor alerts (AGENTS.md rule 2).

pub mod cimb_niaga;
pub mod jenius;
pub mod mandiri_card;
pub mod mandiri_livin;

use chrono::{FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use thiserror::Error;

/// A card charge read from a notification email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCharge {
    /// Merchant exactly as the bank wrote it.
    pub merchant_raw: String,
    pub amount_idr: i64,
    /// When the charge happened, in UTC.
    pub charged_at: NaiveDateTime,
    /// Card digits shown after the mask ("7078" from "XXXX7078"), used to
    /// find the registered card (`cards.card_ref`).
    pub card_ref: String,
    /// Card product name, when the email shows one ("Marriott Bonvoy® Mandiri").
    pub card_name: Option<String>,
}

/// A parser: the plain-text email body in, a charge out.
///
/// `fn(...)` is a plain function pointer type, so each bank's `parse`
/// function can be stored and returned like any other value.
pub type ParseFn = fn(&str) -> Result<ParsedCharge, ParseError>;

/// The parser for emails from `sender` (a bare address), if one exists.
/// Senders without a parser go to the model fallback instead.
pub fn for_sender(sender: &str) -> Option<ParseFn> {
    // Email addresses are case-insensitive in practice.
    match sender.trim().to_ascii_lowercase().as_str() {
        mandiri_livin::SENDER => Some(mandiri_livin::parse),
        mandiri_card::SENDER => Some(mandiri_card::parse),
        cimb_niaga::SENDER => Some(cimb_niaga::parse),
        jenius::SENDER => Some(jenius::parse),
        _ => None,
    }
}

/// A sender that has a parser, described for people (and the model) choosing
/// which sender a card uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownSender {
    /// What the emails are, e.g. "Mandiri credit card".
    pub description: &'static str,
    pub sender: &'static str,
    /// How many card digits the emails show after the mask.
    pub card_digits: usize,
}

/// Every sender `for_sender` has a parser for.
pub const KNOWN_SENDERS: [KnownSender; 4] = [
    KnownSender {
        description: "Mandiri credit card used in the Livin' app (QRIS payments)",
        sender: mandiri_livin::SENDER,
        card_digits: 4,
    },
    KnownSender {
        description: "Mandiri credit card at a terminal or online",
        sender: mandiri_card::SENDER,
        card_digits: 4,
    },
    KnownSender {
        description: "CIMB Niaga credit card",
        sender: cimb_niaga::SENDER,
        card_digits: 2,
    },
    KnownSender {
        description: "Jenius credit card",
        sender: jenius::SENDER,
        card_digits: 4,
    },
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("field {0:?} not found")]
    MissingField(&'static str),
    #[error("field {field:?} has unexpected value {value:?}")]
    BadValue { field: &'static str, value: String },
    #[error("amount {0:?} has non-zero decimals; refusing to round")]
    FractionalAmount(String),
    #[error("payment source {0:?} is not a credit card")]
    NotCreditCard(String),
}

/// The text of an email split into its non-empty pieces.
///
/// Copied or HTML-converted emails put a label and its value either on one
/// line separated by a tab or several spaces ("Jam    13:18:34 WIB"), or on
/// consecutive lines. Splitting on both turns either layout into the same
/// sequence: `["Jam", "13:18:34 WIB", ...]`.
pub struct Tokens<'a> {
    // `'a` says these `&str` slices borrow from the email text passed to
    // `Tokens::new`, so a `Tokens` can't outlive that text.
    items: Vec<&'a str>,
}

impl<'a> Tokens<'a> {
    pub fn new(text: &'a str) -> Self {
        let items = text
            .lines()
            .flat_map(split_columns)
            .map(|piece| piece.trim_matches(|c: char| c.is_whitespace()))
            .filter(|piece| !piece.is_empty())
            .collect();
        Self { items }
    }

    /// The `n` pieces right after the first piece equal to `label`.
    pub fn after(&self, label: &'static str, n: usize) -> Result<&[&'a str], ParseError> {
        let start = self
            .items
            .iter()
            .position(|item| *item == label)
            .ok_or(ParseError::MissingField(label))?
            + 1;
        self.items
            .get(start..start + n)
            .ok_or(ParseError::MissingField(label))
    }

    /// The value for `label`, without the colon some banks put between them.
    /// Handles "Jam  13:18:34 WIB", "Sejumlah  : IDR420.000,00",
    /// "Jumlah  :  126,400.00 IDR" and "Total: IDR 20,500.00" alike.
    pub fn value(&self, label: &'static str) -> Result<&'a str, ParseError> {
        for (i, item) in self.items.iter().enumerate() {
            if *item == label {
                // Label alone: the value is the next piece that isn't a colon.
                return self.items[i + 1..]
                    .iter()
                    .map(|next| next.strip_prefix(':').unwrap_or(next).trim())
                    .find(|next| !next.is_empty())
                    .ok_or(ParseError::MissingField(label));
            }
            // "Label: value" in one piece.
            if let Some(rest) = item.strip_prefix(label)
                && let Some(value) = rest.trim_start().strip_prefix(':')
                && !value.trim().is_empty()
            {
                return Ok(value.trim());
            }
        }
        Err(ParseError::MissingField(label))
    }
}

/// Splits a line on tabs and on runs of two or more spaces (including
/// non-breaking spaces, which HTML emails use for padding).
fn split_columns(line: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut gap_start: Option<usize> = None;
    let mut gap_len = 0;

    for (i, c) in line.char_indices() {
        let is_gap = c == '\t' || c == ' ' || c == '\u{a0}';
        if is_gap {
            if gap_start.is_none() {
                gap_start = Some(i);
                gap_len = 0;
            }
            gap_len += if c == '\t' { 2 } else { 1 };
        } else if let Some(gap) = gap_start.take()
            && gap_len >= 2
        {
            pieces.push(&line[start..gap]);
            start = i;
        }
    }
    pieces.push(&line[start..]);
    pieces
}

/// Parses a rupiah amount in Indonesian format: "Rp 31.200,00",
/// "Rp1.240.000", "IDR420.000,00". Dots group thousands, a comma starts the
/// decimals.
///
/// Each parser says which format its bank uses (see [`parse_rupiah_english`]):
/// "1.234" means 1,234 in one and 1.234 in the other, so we never guess.
pub fn parse_rupiah(field: &'static str, raw: &str) -> Result<i64, ParseError> {
    let trimmed = raw.trim();
    let number = trimmed
        .strip_prefix("Rp")
        .or_else(|| trimmed.strip_prefix("IDR"))
        .ok_or_else(|| bad_value(field, raw))?
        .trim_start_matches('.')
        .trim();
    parse_grouped(field, raw, number, '.', ',')
}

/// Parses a rupiah amount in English format, with "IDR" before or after:
/// "126,400.00 IDR", "IDR 20,500.00". Commas group thousands, a dot starts
/// the decimals.
pub fn parse_rupiah_english(field: &'static str, raw: &str) -> Result<i64, ParseError> {
    let trimmed = raw.trim();
    let number = trimmed
        .strip_prefix("IDR")
        .or_else(|| trimmed.strip_suffix("IDR"))
        .ok_or_else(|| bad_value(field, raw))?
        .trim();
    parse_grouped(field, raw, number, ',', '.')
}

/// Shared by both formats: digits grouped by `thousands`, optionally
/// followed by `decimal` and zeros. Decimals must be zero: rupiah charges
/// have no sen, and a non-zero value means we misread the format.
fn parse_grouped(
    field: &'static str,
    raw: &str,
    number: &str,
    thousands: char,
    decimal: char,
) -> Result<i64, ParseError> {
    let (whole, decimals) = match number.split_once(decimal) {
        Some((whole, decimals)) => (whole, Some(decimals)),
        None => (number, None),
    };
    if let Some(decimals) = decimals {
        if decimals.is_empty() || !decimals.chars().all(|c| c.is_ascii_digit()) {
            return Err(bad_value(field, raw));
        }
        if decimals.chars().any(|c| c != '0') {
            return Err(ParseError::FractionalAmount(raw.to_string()));
        }
    }

    // Thousand groups: the first has 1-3 digits, every later one exactly 3.
    let groups: Vec<&str> = whole.split(thousands).collect();
    let valid_groups = groups.iter().enumerate().all(|(i, group)| {
        let len_ok = if i == 0 {
            (1..=3).contains(&group.len())
        } else {
            group.len() == 3
        };
        len_ok && group.chars().all(|c| c.is_ascii_digit())
    });
    if !valid_groups {
        return Err(bad_value(field, raw));
    }

    // `checked_*` returns `None` on overflow instead of wrapping around.
    let amount = groups.concat().chars().try_fold(0_i64, |acc, c| {
        let digit = i64::from(c.to_digit(10)?);
        acc.checked_mul(10)?.checked_add(digit)
    });
    match amount {
        Some(amount) if amount > 0 => Ok(amount),
        _ => Err(bad_value(field, raw)),
    }
}

pub(crate) fn bad_value(field: &'static str, raw: &str) -> ParseError {
    ParseError::BadValue {
        field,
        value: raw.to_string(),
    }
}

/// The `digits` card digits after the mask: "XXXX7078" -> "7078",
/// "5192-99XX-XXXX-XX19" -> "19", "437896******3572" -> "3572".
/// Each bank always shows the same number of digits, so anything else is
/// refused. Numbers with no mask are refused too, so a full card number is
/// never stored.
pub fn masked_card_ref(
    field: &'static str,
    raw: &str,
    digits: usize,
) -> Result<String, ParseError> {
    let raw_trimmed = raw.trim();
    let is_mask = |c: char| matches!(c, 'X' | 'x' | '*');
    let after_mask = raw_trimmed
        .rfind(is_mask)
        .map(|i| &raw_trimmed[i + 1..])
        .ok_or_else(|| bad_value(field, raw))?;
    let valid = after_mask.len() == digits && after_mask.chars().all(|c| c.is_ascii_digit());
    if valid {
        Ok(after_mask.to_string())
    } else {
        Err(bad_value(field, raw))
    }
}

/// Western Indonesia Time, UTC+7. For banks whose emails give no zone:
/// they are Jakarta banks, and their other emails say WIB.
pub fn wib() -> FixedOffset {
    FixedOffset::east_opt(7 * 3600).expect("UTC+7 is a valid offset")
}

/// Parses a date with a month name, in Indonesian or English:
/// "23 Okt 2026", "23 OCT 2026", "17 Agustus 2026".
pub fn parse_named_date(field: &'static str, raw: &str) -> Result<NaiveDate, ParseError> {
    let bad = || ParseError::BadValue {
        field,
        value: raw.to_string(),
    };
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let [day, month, year] = parts.as_slice() else {
        return Err(bad());
    };
    let month = match month.to_lowercase().as_str() {
        // No name means different months in the two languages, so one
        // table is safe.
        "jan" | "januari" | "january" => 1,
        "feb" | "februari" | "february" => 2,
        "mar" | "maret" | "march" => 3,
        "apr" | "april" => 4,
        "mei" | "may" => 5,
        "jun" | "juni" | "june" => 6,
        "jul" | "juli" | "july" => 7,
        "agu" | "agt" | "agustus" | "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "okt" | "oktober" | "oct" | "october" => 10,
        "nov" | "november" => 11,
        "des" | "desember" | "dec" | "december" => 12,
        _ => return Err(bad()),
    };
    let day: u32 = day.parse().map_err(|_| bad())?;
    let year: i32 = year.parse().map_err(|_| bad())?;
    NaiveDate::from_ymd_opt(year, month, day).ok_or_else(bad)
}

/// Parses a time with an Indonesian zone suffix, like "13:18:34 WIB", and
/// returns it with the zone's UTC offset.
pub fn parse_indonesian_time(
    field: &'static str,
    raw: &str,
) -> Result<(NaiveTime, FixedOffset), ParseError> {
    let bad = || ParseError::BadValue {
        field,
        value: raw.to_string(),
    };
    let (time, zone) = raw.trim().split_once(' ').ok_or_else(bad)?;
    let hours = match zone.trim() {
        "WIB" => 7,
        "WITA" => 8,
        "WIT" => 9,
        _ => return Err(bad()),
    };
    let time = NaiveTime::parse_from_str(time, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(time, "%H:%M"))
        .map_err(|_| bad())?;
    let offset = FixedOffset::east_opt(hours * 3600).ok_or_else(bad)?;
    Ok((time, offset))
}

/// Combines a local date, time and offset into a UTC timestamp.
pub fn to_utc(date: NaiveDate, time: NaiveTime, offset: FixedOffset) -> NaiveDateTime {
    // A fixed offset has no DST gaps, so `.single()` always has one answer.
    offset
        .from_local_datetime(&date.and_time(time))
        .single()
        .expect("fixed offsets map every local time to exactly one instant")
        .naive_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_parser_by_sender() {
        let livin = for_sender("noreply.livin@bankmandiri.co.id").unwrap();
        let card = for_sender("  MandiriCare.OnlineNotif@bankmandiri.co.id ").unwrap();
        let livin_body = include_str!("../../tests/fixtures/mandiri_livin/qris_credit_card.txt");
        let card_body = include_str!("../../tests/fixtures/mandiri_card/charge.txt");
        assert_eq!(livin(livin_body).unwrap().amount_idr, 31_200);
        assert_eq!(card(card_body).unwrap().amount_idr, 420_000);
    }

    #[test]
    fn every_known_sender_has_a_parser() {
        for known in KNOWN_SENDERS {
            assert!(for_sender(known.sender).is_some(), "{}", known.sender);
        }
    }

    #[test]
    fn every_bank_sample_has_a_parser() {
        let samples = [
            (
                "CREDITCARD.NOTIFICATION@cimbniaga.co.id",
                include_str!("../../tests/fixtures/cimb_niaga/charge.txt"),
            ),
            (
                "jenius_noreply@smbci.com",
                include_str!("../../tests/fixtures/jenius/charge.txt"),
            ),
        ];
        for (sender, body) in samples {
            let parse = for_sender(sender).unwrap_or_else(|| panic!("no parser for {sender}"));
            assert!(parse(body).is_ok(), "{sender} sample did not parse");
        }
    }

    #[test]
    fn unknown_sender_has_no_parser() {
        assert!(for_sender("promo@example.com").is_none());
    }

    #[test]
    fn rupiah_with_zero_decimals() {
        assert_eq!(parse_rupiah("amount", "Rp 31.200,00"), Ok(31_200));
        assert_eq!(parse_rupiah("amount", "Rp 1.240.000,00"), Ok(1_240_000));
        assert_eq!(parse_rupiah("amount", "Rp 500,00"), Ok(500));
    }

    #[test]
    fn rupiah_without_space_or_decimals() {
        assert_eq!(parse_rupiah("amount", "Rp1.234.567"), Ok(1_234_567));
        assert_eq!(parse_rupiah("amount", "Rp. 2.500.000"), Ok(2_500_000));
    }

    #[test]
    fn rupiah_in_english_format() {
        let en = |s| parse_rupiah_english("amount", s);
        assert_eq!(en("126,400.00 IDR"), Ok(126_400));
        assert_eq!(en("IDR 20,500.00"), Ok(20_500));
        assert_eq!(en("IDR 1,240,000"), Ok(1_240_000));
        assert_eq!(
            en("IDR 20,500.50"),
            Err(ParseError::FractionalAmount("IDR 20,500.50".into()))
        );
        // Indonesian format given to the English reader is refused, not misread.
        assert!(en("IDR 420.000,00").is_err());
        assert!(en("USD 12.50").is_err());
    }

    #[test]
    fn masked_card_refs() {
        let card = |s, n| masked_card_ref("card", s, n);
        assert_eq!(card("XXXX7078", 4), Ok("7078".into()));
        assert_eq!(card("****7078", 4), Ok("7078".into()));
        assert_eq!(card("5192-99XX-XXXX-XX19", 2), Ok("19".into()));
        assert_eq!(card("437896******3572", 4), Ok("3572".into()));
        assert!(
            card("4378961234563572", 4).is_err(),
            "unmasked numbers are refused"
        );
        assert!(card("XXXX123", 4).is_err());
        assert!(card("XXXX12a4", 4).is_err());
        assert!(card("5192-99XX-XXXX-X119", 2).is_err());
    }

    #[test]
    fn tokens_read_label_and_value_in_one_piece() {
        let tokens = Tokens::new("Total: IDR 20,500.00\nMerchant: GOPAYID 1500729 ID\n");
        assert_eq!(tokens.value("Total"), Ok("IDR 20,500.00"));
        assert_eq!(tokens.value("Merchant"), Ok("GOPAYID 1500729 ID"));
    }

    #[test]
    fn rupiah_with_idr_prefix() {
        assert_eq!(parse_rupiah("amount", "IDR420.000,00"), Ok(420_000));
        assert_eq!(parse_rupiah("amount", "IDR 1.500.000"), Ok(1_500_000));
    }

    #[test]
    fn rupiah_with_non_zero_decimals_is_refused() {
        assert_eq!(
            parse_rupiah("amount", "Rp 31.200,50"),
            Err(ParseError::FractionalAmount("Rp 31.200,50".into()))
        );
    }

    #[test]
    fn rupiah_in_english_format_is_refused() {
        // "1,240,000.00" would silently become 1 if commas were decimals.
        assert!(parse_rupiah("amount", "Rp 1,240,000.00").is_err());
    }

    #[test]
    fn rupiah_with_bad_grouping_is_refused() {
        assert!(parse_rupiah("amount", "Rp 31.20,00").is_err());
        assert!(parse_rupiah("amount", "Rp 1234.567").is_err());
        assert!(parse_rupiah("amount", "Rp .200").is_err());
    }

    #[test]
    fn rupiah_needs_the_currency_prefix() {
        assert!(parse_rupiah("amount", "31.200,00").is_err());
        assert!(parse_rupiah("amount", "USD 31.20").is_err());
    }

    #[test]
    fn rupiah_zero_and_overflow_are_refused() {
        assert!(parse_rupiah("amount", "Rp 0,00").is_err());
        assert!(parse_rupiah("amount", "Rp 99.999.999.999.999.999.999").is_err());
    }

    #[test]
    fn indonesian_dates() {
        let date = |s| parse_named_date("date", s);
        assert_eq!(
            date("23 Okt 2026"),
            Ok(NaiveDate::from_ymd_opt(2026, 10, 23).unwrap())
        );
        assert_eq!(
            date("1 Mei 2026"),
            Ok(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap())
        );
        assert_eq!(
            date("17 Agustus 2026"),
            Ok(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap())
        );
        assert!(date("31 Feb 2026").is_err());
        assert_eq!(
            date("23 OCT 2026"),
            Ok(NaiveDate::from_ymd_opt(2026, 10, 23).unwrap())
        );
        assert!(date("23/10/2026").is_err());
    }

    #[test]
    fn wib_time_converts_to_utc() {
        let (time, offset) = parse_indonesian_time("time", "13:18:34 WIB").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 10, 23).unwrap();
        let utc = to_utc(date, time, offset);
        assert_eq!(utc.to_string(), "2026-10-23 06:18:34");
    }

    #[test]
    fn early_morning_wib_is_previous_day_in_utc() {
        let (time, offset) = parse_indonesian_time("time", "02:00:00 WIB").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 10, 23).unwrap();
        assert_eq!(
            to_utc(date, time, offset).to_string(),
            "2026-10-22 19:00:00"
        );
    }

    #[test]
    fn unknown_zone_is_refused() {
        assert!(parse_indonesian_time("time", "13:18:34 GMT").is_err());
        assert!(parse_indonesian_time("time", "13:18:34").is_err());
    }

    #[test]
    fn tokens_handle_same_line_and_next_line_layouts() {
        let same_line = Tokens::new("Jam    13:18:34 WIB\n");
        let tabbed = Tokens::new("Jam\t13:18:34 WIB\n");
        let next_line = Tokens::new("Jam\n\n  13:18:34 WIB  \n");
        for tokens in [same_line, tabbed, next_line] {
            assert_eq!(tokens.value("Jam"), Ok("13:18:34 WIB"));
        }
    }

    #[test]
    fn tokens_keep_single_spaces_inside_values() {
        let tokens = Tokens::new("Penerima\nMichie Laundry\n");
        assert_eq!(tokens.value("Penerima"), Ok("Michie Laundry"));
    }

    #[test]
    fn tokens_strip_colon_separators() {
        let attached = Tokens::new("Sejumlah    : IDR420.000,00\n");
        let spaced = Tokens::new("Jumlah Transaksi    :    126,400.00 IDR\n");
        assert_eq!(attached.value("Sejumlah"), Ok("IDR420.000,00"));
        assert_eq!(spaced.value("Jumlah Transaksi"), Ok("126,400.00 IDR"));
    }

    #[test]
    fn missing_label_is_reported_by_name() {
        let tokens = Tokens::new("Tanggal    23 Okt 2026\n");
        assert_eq!(tokens.value("Jam"), Err(ParseError::MissingField("Jam")));
    }
}
