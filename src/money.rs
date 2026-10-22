//! Money as a whole number of cents.
//!
//! Floats can't represent most decimal amounts exactly (0.1 + 0.2 != 0.3 in binary
//! floating point), so amounts are never stored or added up as floats. A `Money` is
//! an `i64` of cents wrapped in its own type, so the compiler stops us from mixing
//! it up with other numbers such as counts or event sequence numbers.

use std::fmt;
use std::ops::{Add, Sub};

use duckdb::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef};
use serde::{Deserialize, Serialize};

/// An amount in cents. Negative means money going out.
///
/// `#[serde(transparent)]` stores it in JSON as a plain number, e.g. `-5890`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Money(pub i64);

impl Money {
    pub const ZERO: Money = Money(0);

    /// True for money coming in (salary, refunds), false for money going out.
    pub fn is_income(self) -> bool {
        self.0 > 0
    }

    /// This amount as a percentage of `total` (0 when `total` is zero).
    pub fn percent_of(self, total: Money) -> f64 {
        if total.0 == 0 {
            0.0
        } else {
            self.0 as f64 / total.0 as f64 * 100.0
        }
    }

    /// Parses an amount as banks write it: "-58.90", "-23,50", "1.234,56" or
    /// "1,234.56". Rounds half up to whole cents. Works on the digits as text, so
    /// no float rounding is involved.
    pub fn parse(text: &str) -> Option<Money> {
        let normalized = normalize_decimal_separator(text);
        let (negative, digits) = match normalized.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, normalized.strip_prefix('+').unwrap_or(&normalized)),
        };
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if (whole.is_empty() && fraction.is_empty()) || !all_digits(whole) || !all_digits(fraction) {
            return None;
        }

        let whole: i64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
        // Keep three decimal places, then round the third away: "2.995" -> 300 cents.
        let mut thousandths: String = fraction.chars().take(3).collect();
        while thousandths.len() < 3 {
            thousandths.push('0');
        }
        let cents = whole.checked_mul(100)? + (thousandths.parse::<i64>().ok()? + 5) / 10;
        Some(Money(if negative { -cents } else { cents }))
    }
}

/// Returns the amount with '.' as the decimal separator and no thousands separators.
fn normalize_decimal_separator(text: &str) -> String {
    let s = text.trim().replace(' ', "");
    match (s.rfind(','), s.rfind('.')) {
        // Brazilian style "1.234,56": the comma comes last, so it's the decimal point.
        (Some(comma), Some(dot)) if comma > dot => s.replace('.', "").replace(',', "."),
        // US style "1,234.56".
        (Some(_), Some(_)) => s.replace(',', ""),
        // Only a comma: "-23,50".
        (Some(_), None) => s.replace(',', "."),
        _ => s,
    }
}

/// Formats as "1,234.56" (or "-1,234.56"). Honors width and alignment, so
/// `format!("{:>10}", money)` right-aligns it.
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let abs = self.0.unsigned_abs();
        let whole = (abs / 100).to_string();
        let mut grouped = String::new();
        for (i, digit) in whole.chars().enumerate() {
            if i > 0 && (whole.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        let sign = if self.0 < 0 { "-" } else { "" };
        f.pad(&format!("{sign}{grouped}.{:02}", abs % 100))
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, other: Money) -> Money {
        Money(self.0 + other.0)
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, other: Money) -> Money {
        Money(self.0 - other.0)
    }
}

/// Lets a `Money` be passed straight into a DuckDB query (as a BIGINT of cents).
impl ToSql for Money {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(Value::BigInt(self.0)))
    }
}

/// Lets a BIGINT column of cents be read straight into a `Money`.
impl FromSql for Money {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(Money)
    }
}

#[cfg(test)]
mod tests {
    use super::Money;

    #[test]
    fn formats_money() {
        assert_eq!(Money(0).to_string(), "0.00");
        assert_eq!(Money(5).to_string(), "0.05");
        assert_eq!(Money(-5890).to_string(), "-58.90");
        assert_eq!(Money(123_456_789).to_string(), "1,234,567.89");
        assert_eq!(format!("{:>8}", Money(1050)), "   10.50");
    }

    #[test]
    fn parses_exact_cents() {
        assert_eq!(Money::parse("-58.90"), Some(Money(-5890)));
        assert_eq!(Money::parse("-23,50"), Some(Money(-2350)));
        assert_eq!(Money::parse("1.234,56"), Some(Money(123_456)));
        assert_eq!(Money::parse("1,234.56"), Some(Money(123_456)));
        assert_eq!(Money::parse("12"), Some(Money(1200)));
        assert_eq!(Money::parse("-0,5"), Some(Money(-50)));
        assert_eq!(Money::parse("0.1"), Some(Money(10)));
        assert_eq!(Money::parse("2.995"), Some(Money(300)));
        assert_eq!(Money::parse("abc"), None);
        assert_eq!(Money::parse(""), None);
    }

    #[test]
    fn arithmetic_and_percentages() {
        assert_eq!(Money(300_000) - Money(24_740), Money(275_260));
        assert_eq!(Money(100) + Money(-30), Money(70));
        assert!((Money(12_000).percent_of(Money(24_740)) - 48.5).abs() < 0.05);
        assert_eq!(Money(5).percent_of(Money::ZERO), 0.0);
        assert!(Money(1).is_income() && !Money(-1).is_income());
    }
}
