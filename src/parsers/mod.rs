//! Deterministic parsers for bank notification emails, one module per bank
//! template. The model never reads amounts (AGENTS.md rule 2): if a parser
//! cannot read an email exactly, it returns an error and the ingestor alerts.

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
        _ => None,
    }
}

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

    /// The value right after `label`, without the colon some banks put
    /// between them. Handles "Jam  13:18:34 WIB", "Sejumlah  : IDR420.000,00"
    /// and "Jumlah  :  126,400.00 IDR" alike.
    pub fn value(&self, label: &'static str) -> Result<&'a str, ParseError> {
        let start = self
            .items
            .iter()
            .position(|item| *item == label)
            .ok_or(ParseError::MissingField(label))?;
        self.items[start + 1..]
            .iter()
            .map(|item| item.strip_prefix(':').unwrap_or(item).trim())
            .find(|item| !item.is_empty())
            .ok_or(ParseError::MissingField(label))
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

/// Parses an Indonesian-formatted rupiah amount: "Rp 31.200,00",
/// "Rp1.240.000", "IDR420.000,00". Dots group thousands, a comma starts the
/// decimals. Banks that write "126,400.00" (English format) need their own
/// helper: the two formats can't be told apart reliably, so each parser
/// states which one its bank uses. Decimals must be zero: rupiah charges have no sen, and a
/// non-zero value means we misread the format.
pub fn parse_rupiah(field: &'static str, raw: &str) -> Result<i64, ParseError> {
    let bad = || ParseError::BadValue {
        field,
        value: raw.to_string(),
    };

    let trimmed = raw.trim();
    let number = trimmed
        .strip_prefix("Rp")
        .or_else(|| trimmed.strip_prefix("IDR"))
        .ok_or_else(bad)?
        .trim_start_matches('.')
        .trim();

    let (whole, decimals) = match number.split_once(',') {
        Some((whole, decimals)) => (whole, Some(decimals)),
        None => (number, None),
    };
    if let Some(decimals) = decimals {
        if decimals.is_empty() || !decimals.chars().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        if decimals.chars().any(|c| c != '0') {
            return Err(ParseError::FractionalAmount(raw.to_string()));
        }
    }

    // Thousand groups: the first has 1-3 digits, every later one exactly 3.
    let groups: Vec<&str> = whole.split('.').collect();
    let valid_groups = groups.iter().enumerate().all(|(i, group)| {
        let len_ok = if i == 0 {
            (1..=3).contains(&group.len())
        } else {
            group.len() == 3
        };
        len_ok && group.chars().all(|c| c.is_ascii_digit())
    });
    if !valid_groups {
        return Err(bad());
    }

    // `checked_*` returns `None` on overflow instead of wrapping around.
    let amount = groups.concat().chars().try_fold(0_i64, |acc, c| {
        let digit = i64::from(c.to_digit(10)?);
        acc.checked_mul(10)?.checked_add(digit)
    });
    match amount {
        Some(amount) if amount > 0 => Ok(amount),
        _ => Err(bad()),
    }
}

/// Parses an Indonesian date like "23 Okt 2026".
pub fn parse_indonesian_date(field: &'static str, raw: &str) -> Result<NaiveDate, ParseError> {
    let bad = || ParseError::BadValue {
        field,
        value: raw.to_string(),
    };
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let [day, month, year] = parts.as_slice() else {
        return Err(bad());
    };
    let month = match month.to_lowercase().as_str() {
        "jan" | "januari" => 1,
        "feb" | "februari" => 2,
        "mar" | "maret" => 3,
        "apr" | "april" => 4,
        "mei" => 5,
        "jun" | "juni" => 6,
        "jul" | "juli" => 7,
        "agu" | "agt" | "agustus" => 8,
        "sep" | "september" => 9,
        "okt" | "oktober" => 10,
        "nov" | "november" => 11,
        "des" | "desember" => 12,
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
    fn unknown_sender_has_no_parser() {
        assert!(for_sender("CREDITCARD.NOTIFICATION@cimbniaga.co.id").is_none());
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
        let date = |s| parse_indonesian_date("date", s);
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
        assert!(date("23 Oct 2026").is_err());
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
