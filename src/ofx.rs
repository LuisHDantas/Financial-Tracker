//! OFX statement parsing. Handles SGML OFX (Itaú's "OFX / Money 2000" export, which
//! has no closing tags) and XML OFX. Amounts become integer cents, never floats.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::events::AccountType;

pub struct Statement {
    pub account: String,
    pub account_type: AccountType,
    pub transactions: Vec<ParsedTransaction>,
}

pub struct ParsedTransaction {
    pub id: String,
    pub fitid: String,
    pub date: String, // YYYY-MM-DD
    pub amount_cents: i64,
    pub memo: String,
    pub trn_type: Option<String>,
}

pub fn parse(bytes: &[u8]) -> Statement {
    let text = decode(bytes);
    let upper = text.to_ascii_uppercase(); // same byte offsets as `text`
    // ACCTID sits in BANKACCTFROM (checking) or CCACCTFROM (credit card).
    let account = tag(&text, "ACCTID").unwrap_or_else(|| "unknown".into());
    let account_type = if upper.contains("<CCSTMTRS>") || upper.contains("<CCACCTFROM>") {
        AccountType::CreditCard
    } else {
        AccountType::Checking
    };

    let mut transactions = Vec::new();
    for block in blocks(&text, &upper) {
        let (Some(posted), Some(amount)) = (tag(block, "DTPOSTED"), tag(block, "TRNAMT")) else {
            continue;
        };
        let (Some(date), Some(amount_cents)) = (parse_date(&posted), parse_cents(&amount)) else {
            continue;
        };
        let memo = tag(block, "MEMO")
            .filter(|m| !m.is_empty())
            .or_else(|| tag(block, "NAME"))
            .unwrap_or_default();
        let memo = memo.split_whitespace().collect::<Vec<_>>().join(" ");
        let fitid = tag(block, "FITID").unwrap_or_default();
        let key = format!("{account}|{fitid}|{date}|{amount_cents}|{memo}");
        transactions.push(ParsedTransaction {
            id: format!("{:x}", Sha256::digest(key.as_bytes()))[..24].to_string(),
            fitid,
            date,
            amount_cents,
            memo,
            trn_type: tag(block, "TRNTYPE"),
        });
    }
    Statement { account, account_type, transactions }
}

fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        // Brazilian banks often export Windows-1252 (Latin-1).
        Err(_) => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

/// The text of every <STMTTRN> block, up to the next one or the end of the list.
fn blocks<'a>(text: &'a str, upper: &str) -> Vec<&'a str> {
    const OPEN: &str = "<STMTTRN>";
    let starts: Vec<usize> = upper.match_indices(OPEN).map(|(i, _)| i + OPEN.len()).collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let mut end = starts.get(k + 1).map_or(text.len(), |&next| next - OPEN.len());
            for close in ["</BANKTRANLIST>", "</CCSTMTRS>"] {
                if let Some(i) = upper[start..end].find(close) {
                    end = start + i;
                }
            }
            &text[start..end]
        })
        .collect()
}

/// Value of `<NAME>value`. Works for SGML (no closing tag) and XML.
fn tag(block: &str, name: &str) -> Option<String> {
    const TAGS: [&str; 7] = ["ACCTID", "DTPOSTED", "TRNAMT", "MEMO", "NAME", "FITID", "TRNTYPE"];
    static PATTERNS: OnceLock<HashMap<&'static str, Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        TAGS.iter()
            .map(|t| (*t, Regex::new(&format!(r"(?i)<{t}>([^<\r\n]*)")).unwrap()))
            .collect()
    });
    patterns[name].captures(block).map(|c| c[1].trim().to_string())
}

/// "20261005120000[-3:BRT]" -> "2026-10-05"
fn parse_date(posted: &str) -> Option<String> {
    let d = posted.get(..8)?;
    if !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (month, day): (u32, u32) = (d[4..6].parse().ok()?, d[6..8].parse().ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]))
}

/// "-58.90", "-23,50", "1.234,56" or "1,234.56" -> cents, rounding half up.
fn parse_cents(text: &str) -> Option<i64> {
    let mut s = text.trim().replace(' ', "");
    match (s.rfind(','), s.rfind('.')) {
        (Some(comma), Some(dot)) if comma > dot => s = s.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => s = s.replace(',', ""),
        (Some(_), None) => s = s.replace(',', "."),
        _ => {}
    }
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(&s)),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !all_digits(int) || !all_digits(frac) {
        return None;
    }
    let int: i64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let mut frac3: String = frac.chars().take(3).collect();
    while frac3.len() < 3 {
        frac3.push('0');
    }
    let cents = int.checked_mul(100)? + (frac3.parse::<i64>().ok()? + 5) / 10;
    Some(if negative { -cents } else { cents })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SGML: &str = "OFXHEADER:100\nDATA:OFXSGML\n\n<OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS>\n\
        <BANKACCTFROM><BANKID>0341<ACCTID>12345-6</BANKACCTFROM>\n<BANKTRANLIST>\n\
        <STMTTRN>\n<TRNTYPE>DEBIT\n<DTPOSTED>20261005120000[-3:BRT]\n<TRNAMT>-58.90\n<FITID>A1\n<MEMO>IFOOD   *RESTAURANTE\n\
        <STMTTRN>\n<TRNTYPE>CREDIT\n<DTPOSTED>20261006\n<TRNAMT>1500.00\n<FITID>A2\n<MEMO>PAGTO SALARIO\n\
        </BANKTRANLIST>\n<LEDGERBAL><BALAMT>1441.10<DTASOF>20261031\n</STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>\n";

    #[test]
    fn parses_sgml_without_closing_tags() {
        let s = parse(SGML.as_bytes());
        assert_eq!(s.account, "12345-6");
        assert_eq!(s.account_type, AccountType::Checking);
        assert_eq!(s.transactions.len(), 2);
        let t = &s.transactions[0];
        assert_eq!(t.date, "2026-10-05");
        assert_eq!(t.amount_cents, -5890);
        assert_eq!(t.memo, "IFOOD *RESTAURANTE");
        assert_eq!(t.fitid, "A1");
        assert_eq!(t.trn_type.as_deref(), Some("DEBIT"));
        assert_eq!(s.transactions[1].amount_cents, 150_000);
    }

    #[test]
    fn parses_xml() {
        let xml = "<OFX><BANKACCTFROM><ACCTID>999</ACCTID></BANKACCTFROM><BANKTRANLIST>\
            <STMTTRN><TRNTYPE>DEBIT</TRNTYPE><DTPOSTED>20261007</DTPOSTED><TRNAMT>-23.50</TRNAMT>\
            <FITID>B1</FITID><NAME>PADARIA</NAME></STMTTRN></BANKTRANLIST></OFX>";
        let s = parse(xml.as_bytes());
        assert_eq!(s.account, "999");
        assert_eq!(s.transactions.len(), 1);
        assert_eq!(s.transactions[0].memo, "PADARIA");
        assert_eq!(s.transactions[0].amount_cents, -2350);
    }

    #[test]
    fn parses_credit_card_statements() {
        let cc = "OFXHEADER:100\nDATA:OFXSGML\n\n<OFX><CREDITCARDMSGSRSV1><CCSTMTTRNRS><CCSTMTRS>\n\
            <CURDEF>BRL\n<CCACCTFROM><ACCTID>5555XXXXXXXX1234</CCACCTFROM>\n<BANKTRANLIST>\n\
            <STMTTRN>\n<TRNTYPE>DEBIT\n<DTPOSTED>20261003\n<TRNAMT>-120.00\n<FITID>CC1\n<MEMO>RESTAURANTE BOM SABOR\n\
            <STMTTRN>\n<TRNTYPE>CREDIT\n<DTPOSTED>20261010\n<TRNAMT>950.00\n<FITID>CC2\n<MEMO>PAGAMENTO EFETUADO\n\
            </BANKTRANLIST>\n</CCSTMTRS></CCSTMTTRNRS></CREDITCARDMSGSRSV1></OFX>\n";
        let s = parse(cc.as_bytes());
        assert_eq!(s.account, "5555XXXXXXXX1234");
        assert_eq!(s.account_type, AccountType::CreditCard);
        assert_eq!(s.transactions.len(), 2);
        assert_eq!(s.transactions[0].amount_cents, -12_000); // purchases are money out
        assert_eq!(s.transactions[0].memo, "RESTAURANTE BOM SABOR");
        assert_eq!(s.transactions[1].amount_cents, 95_000);
    }

    #[test]
    fn decodes_windows_1252_accents_and_comma_decimals() {
        let mut bytes = b"<ACCTID>1<BANKTRANLIST><STMTTRN><DTPOSTED>20261008<TRNAMT>-23,50<FITID>C1<MEMO>PADARIA S".to_vec();
        bytes.push(0xC3); // 'Ã' in Windows-1252, invalid as UTF-8 here
        bytes.extend(b"O JO");
        bytes.push(0xC3);
        bytes.extend(b"O</BANKTRANLIST>");
        let s = parse(&bytes);
        assert_eq!(s.transactions[0].memo, "PADARIA SÃO JOÃO");
        assert_eq!(s.transactions[0].amount_cents, -2350);
    }

    #[test]
    fn amounts_are_exact_cents() {
        assert_eq!(parse_cents("-58.90"), Some(-5890));
        assert_eq!(parse_cents("-23,50"), Some(-2350));
        assert_eq!(parse_cents("1.234,56"), Some(123_456));
        assert_eq!(parse_cents("1,234.56"), Some(123_456));
        assert_eq!(parse_cents("12"), Some(1200));
        assert_eq!(parse_cents("-0,5"), Some(-50));
        assert_eq!(parse_cents("0.1"), Some(10));
        assert_eq!(parse_cents("2.995"), Some(300));
        assert_eq!(parse_cents("abc"), None);
        assert_eq!(parse_cents(""), None);
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = parse(SGML.as_bytes());
        let b = parse(SGML.as_bytes());
        assert_eq!(a.transactions[0].id, b.transactions[0].id);
        assert_ne!(a.transactions[0].id, a.transactions[1].id);
        assert_eq!(a.transactions[0].id.len(), 24);
    }

    #[test]
    fn skips_blocks_without_date_or_amount() {
        let s = parse(b"<BANKTRANLIST><STMTTRN><MEMO>NO DATE<TRNAMT>-1.00</BANKTRANLIST>");
        assert!(s.transactions.is_empty());
    }
}
