//! Reading OFX statement files.
//!
//! OFX comes in two flavors: SGML (what Itaú's "OFX / Money 2000" export uses),
//! where tags are often never closed (`<TRNAMT>-58.90` and then the next tag), and
//! XML, where they are. Reading each value up to the next `<` or line break works
//! for both, so there's no full OFX parser here.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::events::AccountType;
use crate::money::Money;

/// One statement file: the account it belongs to and its transactions.
pub struct Statement {
    pub account: String,
    pub account_type: AccountType,
    pub transactions: Vec<ParsedTransaction>,
}

pub struct ParsedTransaction {
    /// Stable id: the same transaction gets the same id in every export.
    pub id: String,
    pub fitid: String,
    pub date: String, // YYYY-MM-DD
    pub amount: Money,
    pub memo: String,
    pub trn_type: Option<String>,
    /// The bank's own category for the purchase, when the statement prints one
    /// (Itaú card bills do, in lowercase under each purchase). Never in OFX.
    pub bank_category: Option<String>,
}

/// Parses a statement. Transactions without a usable date or amount are skipped.
pub fn parse(bytes: &[u8]) -> Statement {
    let text = decode(bytes);
    // ASCII uppercasing keeps every byte offset the same as in `text`.
    let upper = text.to_ascii_uppercase();
    // ACCTID sits in BANKACCTFROM (checking) or CCACCTFROM (credit card).
    let account = tag(&text, "ACCTID").unwrap_or_else(|| "unknown".into());
    let account_type = if upper.contains("<CCSTMTRS>") || upper.contains("<CCACCTFROM>") {
        AccountType::CreditCard
    } else {
        AccountType::Checking
    };
    let transactions = transaction_blocks(&text, &upper)
        .into_iter()
        .filter_map(|block| parse_transaction(block, &account))
        .collect();
    Statement { account, account_type, transactions }
}

fn parse_transaction(block: &str, account: &str) -> Option<ParsedTransaction> {
    let date = parse_date(&tag(block, "DTPOSTED")?)?;
    let amount = Money::parse(&tag(block, "TRNAMT")?)?;
    let memo =
        tag(block, "MEMO").filter(|m| !m.is_empty()).or_else(|| tag(block, "NAME")).unwrap_or_default();
    let memo = memo.split_whitespace().collect::<Vec<_>>().join(" ");
    let fitid = tag(block, "FITID").unwrap_or_default();
    Some(ParsedTransaction {
        id: transaction_id(account, &fitid, &date, amount, &memo),
        fitid,
        date,
        amount,
        memo,
        trn_type: tag(block, "TRNTYPE"),
        bank_category: None,
    })
}

/// First 24 hex characters of sha256("account|fitid|date|cents|memo"). Also used
/// for PDFs, where the "fitid" is a count of identical rows.
pub(crate) fn transaction_id(account: &str, fitid: &str, date: &str, amount: Money, memo: &str) -> String {
    let key = format!("{account}|{fitid}|{date}|{}|{memo}", amount.0);
    format!("{:x}", Sha256::digest(key.as_bytes()))[..24].to_string()
}

/// UTF-8 if it is valid UTF-8, otherwise Windows-1252, which Brazilian banks often use.
fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

/// The text of every `<STMTTRN>` block, up to the next one or the end of the list.
fn transaction_blocks<'a>(text: &'a str, upper: &str) -> Vec<&'a str> {
    const OPEN: &str = "<STMTTRN>";
    const LIST_ENDS: [&str; 2] = ["</BANKTRANLIST>", "</CCSTMTRS>"];
    let starts: Vec<usize> = upper.match_indices(OPEN).map(|(i, _)| i + OPEN.len()).collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let mut end = starts.get(k + 1).map_or(text.len(), |&next| next - OPEN.len());
            for close in LIST_ENDS {
                if let Some(i) = upper[start..end].find(close) {
                    end = start + i;
                }
            }
            &text[start..end]
        })
        .collect()
}

/// The value after `<NAME>`, trimmed. Works with and without closing tags.
fn tag(block: &str, name: &str) -> Option<String> {
    const TAGS: [&str; 7] = ["ACCTID", "DTPOSTED", "TRNAMT", "MEMO", "NAME", "FITID", "TRNTYPE"];
    // Compiled once, the first time any tag is read.
    static PATTERNS: OnceLock<HashMap<&'static str, Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        TAGS.iter()
            .map(|t| (*t, Regex::new(&format!(r"(?i)<{t}>([^<\r\n]*)")).expect("valid tag pattern")))
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
        assert_eq!(t.amount, Money(-5890));
        assert_eq!(t.memo, "IFOOD *RESTAURANTE");
        assert_eq!(t.fitid, "A1");
        assert_eq!(t.trn_type.as_deref(), Some("DEBIT"));
        assert_eq!(s.transactions[1].amount, Money(150_000));
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
        assert_eq!(s.transactions[0].amount, Money(-2350));
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
        assert_eq!(s.transactions[0].amount, Money(-12_000)); // purchases are money out
        assert_eq!(s.transactions[0].memo, "RESTAURANTE BOM SABOR");
        assert_eq!(s.transactions[1].amount, Money(95_000));
    }

    #[test]
    fn decodes_windows_1252_accents_and_comma_decimals() {
        let mut bytes =
            b"<ACCTID>1<BANKTRANLIST><STMTTRN><DTPOSTED>20261008<TRNAMT>-23,50<FITID>C1<MEMO>PADARIA S"
                .to_vec();
        bytes.push(0xC3); // 'Ã' in Windows-1252, invalid as UTF-8 here
        bytes.extend(b"O JO");
        bytes.push(0xC3);
        bytes.extend(b"O</BANKTRANLIST>");
        let s = parse(&bytes);
        assert_eq!(s.transactions[0].memo, "PADARIA SÃO JOÃO");
        assert_eq!(s.transactions[0].amount, Money(-2350));
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

    #[test]
    fn rejects_impossible_dates() {
        assert_eq!(parse_date("20261005"), Some("2026-10-05".into()));
        assert_eq!(parse_date("20261305"), None);
        assert_eq!(parse_date("2026-10-"), None);
        assert_eq!(parse_date("2026"), None);
    }
}
