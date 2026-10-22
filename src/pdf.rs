//! Reading PDF statements (Itaú checking statements and card bills).
//!
//! A PDF has no transaction structure like OFX, only text placed on a page. We let
//! `pdftotext -layout` (poppler, installed in the Docker image) turn it into lines
//! that keep the page's columns, then find transactions in those lines with the
//! regular expressions in parsers.toml. The patterns live in that file, not in the
//! code, so a layout can be adjusted without rebuilding.
//!
//! `inspect` prints the *shape* of each line (letters become a/A, digits 9) so a
//! layout can be fixed from someone's statement without anyone seeing its content.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::events::AccountType;
use crate::money::Money;
use crate::ofx::{transaction_id, ParsedTransaction, Statement};

#[derive(Deserialize)]
struct ParsersFile {
    pdf: BTreeMap<String, LayoutDef>,
}

#[derive(Deserialize)]
struct LayoutDef {
    account_type: AccountType,
    detect: Vec<String>,
    transaction: String,
    account: Option<String>,
    skip: Option<String>,
    stop: Option<String>,
    #[serde(default)]
    purchases_positive: bool,
}

/// One bank statement layout from parsers.toml, with its patterns compiled.
pub struct Layout {
    pub name: String,
    pub account_type: AccountType,
    detect: Vec<String>,
    transaction: Regex,
    account: Option<Regex>,
    skip: Option<Regex>,
    stop: Option<Regex>,
    purchases_positive: bool,
}

/// All the layouts in parsers.toml.
pub struct Layouts(Vec<Layout>);

impl Layouts {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: ParsersFile = toml::from_str(text)?;
        let mut layouts = Vec::new();
        for (name, def) in file.pdf {
            let transaction = pattern(&def.transaction, &name, "transaction")?;
            for group in ["date", "description", "amount"] {
                if !transaction.capture_names().any(|n| n == Some(group)) {
                    bail!("[pdf.{name}] `transaction` needs a (?P<{group}>...) group");
                }
            }
            let optional = |p: &Option<String>, key| p.as_deref().map(|p| pattern(p, &name, key)).transpose();
            layouts.push(Layout {
                account: optional(&def.account, "account")?,
                skip: optional(&def.skip, "skip")?,
                stop: optional(&def.stop, "stop")?,
                detect: def.detect.iter().map(|k| k.to_uppercase()).collect(),
                account_type: def.account_type,
                purchases_positive: def.purchases_positive,
                transaction,
                name,
            });
        }
        if layouts.is_empty() {
            bail!("no [pdf.*] layouts");
        }
        Ok(Self(layouts))
    }

    /// How many of each layout's keywords appear in `text`, most first.
    pub fn keyword_hits(&self, text: &str) -> Vec<(&Layout, usize)> {
        let upper = text.to_uppercase();
        let mut hits: Vec<_> = self
            .0
            .iter()
            .map(|l| (l, l.detect.iter().map(|k| upper.matches(k.as_str()).count()).sum::<usize>()))
            .collect();
        hits.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        hits
    }

    /// The layout that fits `text` best, if any of its keywords appear at all.
    pub fn detect(&self, text: &str) -> Option<&Layout> {
        self.keyword_hits(text).into_iter().find(|(_, n)| *n > 0).map(|(layout, _)| layout)
    }
}

fn pattern(p: &str, layout: &str, key: &str) -> Result<Regex> {
    RegexBuilder::new(p)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("[pdf.{layout}] bad `{key}` pattern"))
}

/// The PDF's text with its layout kept, one string with "\n" between lines and
/// "\x0c" (form feed) between pages.
pub fn extract_text(path: &Path) -> Result<String> {
    let output = Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(path)
        .arg("-")
        .output()
        .context("running pdftotext (it's installed in the Docker image: use ./finance)")?;
    if !output.status.success() {
        bail!("pdftotext failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads a statement's text with `layout`.
pub fn parse(text: &str, layout: &Layout) -> Statement {
    let reference = latest_full_date(text);
    let account = layout
        .account
        .as_ref()
        .and_then(|re| re.captures(text))
        .and_then(|c| c.name("account").map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| layout.name.clone());

    // PDFs have no transaction ids, so identical rows (two R$ 5,00 coffees on one
    // day) are told apart by how many came before them in the file.
    let mut seen: HashMap<(String, i64, String), usize> = HashMap::new();
    let mut transactions = Vec::new();
    for line in text.lines() {
        if layout.stop.as_ref().is_some_and(|re| re.is_match(line)) {
            break;
        }
        for caps in transactions_in_line(layout, line) {
            let memo = collapse_spaces(&caps["description"]);
            let Some(date) = full_date(&caps["date"], reference) else { continue };
            let Some(mut amount) = parse_amount(&caps["amount"], caps.name("dc").map(|m| m.as_str())) else {
                continue;
            };
            if layout.purchases_positive {
                amount = Money(-amount.0);
            }
            let occurrence = seen.entry((date.clone(), amount.0, memo.clone())).or_insert(0);
            *occurrence += 1;
            let fitid = format!("pdf#{occurrence}");
            transactions.push(ParsedTransaction {
                id: transaction_id(&account, &fitid, &date, amount, &memo),
                fitid,
                date,
                amount,
                memo,
                trn_type: None,
            });
        }
    }
    Statement { account, account_type: layout.account_type, transactions }
}

/// The transactions `layout` finds in one line, leaving out the `skip` ones
/// (balances, totals). Shared by `parse` and `inspect`, so both agree.
fn transactions_in_line<'a>(layout: &'a Layout, line: &'a str) -> impl Iterator<Item = regex::Captures<'a>> {
    layout.transaction.captures_iter(line).filter(|caps| {
        let memo = collapse_spaces(&caps["description"]);
        !layout.skip.as_ref().is_some_and(|re| re.is_match(&memo))
    })
}

fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "-1.234,56", "1.234,56-", "R$ 12,00", plus an optional D (debit) / C (credit) mark.
fn parse_amount(text: &str, debit_credit: Option<&str>) -> Option<Money> {
    let mut s: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    s = s.replace("R$", "");
    let trailing_minus = s.ends_with('-') && !s.starts_with('-');
    let amount = Money::parse(s.trim_end_matches('-'))?;
    let amount = if trailing_minus { Money(-amount.0) } else { amount };
    Some(match debit_credit.map(str::to_uppercase).as_deref() {
        Some("D") => Money(-amount.0.abs()),
        Some("C") => Money(amount.0.abs()),
        _ => amount,
    })
}

/// The latest DD/MM/YYYY date in the text, as (year, month): the reference for dates
/// printed without a year.
fn latest_full_date(text: &str) -> Option<(i32, u32)> {
    let re = Regex::new(r"\b(\d{2})/(\d{2})/(\d{4})\b").expect("valid pattern");
    re.captures_iter(text)
        .filter_map(|c| {
            let (day, month, year): (u32, u32, i32) =
                (c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?);
            ((1..=31).contains(&day) && (1..=12).contains(&month)).then_some((year, month))
        })
        .max()
}

/// "05/10/2026", "05/10/26" or "05/10" (year from `reference`) -> "2026-10-05".
fn full_date(text: &str, reference: Option<(i32, u32)>) -> Option<String> {
    let mut parts = text.split('/');
    let day: u32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let year: i32 = match parts.next() {
        Some(y) if y.len() == 4 => y.parse().ok()?,
        Some(y) if y.len() == 2 => 2000 + y.parse::<i32>().ok()?,
        Some(_) => return None,
        None => {
            let (ref_year, ref_month) = reference?;
            if month > ref_month {
                ref_year - 1
            } else {
                ref_year
            }
        }
    };
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

// --- inspect -------------------------------------------------------------------

/// A report on a statement's text that shows no content: each line's shape, which
/// lines the detected layout reads as transactions (✓, or ✓2 for two in one line),
/// where it stops reading (■), and the keyword counts.
pub fn inspect(text: &str, layouts: &Layouts) -> String {
    let mut out = String::from(
        "Line shapes only: letters became a/A, digits 9. Nothing from the file itself is shown.\n\
         ✓ = read as a transaction (✓2 = two in one line), ■ = reading stops here.\n",
    );
    let hits = layouts.keyword_hits(text);
    let counts: Vec<String> = hits.iter().map(|(l, n)| format!("{} {n}", l.name)).collect();
    let layout = layouts.detect(text);
    out += &format!(
        "Looks like: {} (keyword hits: {})\n",
        layout.map_or("nothing known", |l| l.name.as_str()),
        counts.join(", ")
    );
    if let Some(layout) = layout {
        let statement = parse(text, layout);
        out += &format!("Transactions found: {}\n", statement.transactions.len());
    }

    // Consecutive lines with the same shape and mark are shown once, with a count.
    let mut previous: Option<(String, String)> = None; // (mark, shape)
    let mut run: Vec<usize> = Vec::new();
    let flush = |out: &mut String, previous: &Option<(String, String)>, run: &mut Vec<usize>| {
        if let (Some((mark, shape)), Some(first)) = (previous, run.first()) {
            let lines = if run.len() == 1 {
                format!("{first}")
            } else {
                format!("{first}-{} ({}x)", run[run.len() - 1], run.len())
            };
            *out += &format!("{mark:<2} {lines:>12}  {shape}\n");
        }
        run.clear();
    };
    let mut stopped = false;
    for (p, page) in text.split('\x0c').enumerate() {
        if page.trim().is_empty() {
            continue;
        }
        flush(&mut out, &previous, &mut run);
        previous = None;
        out += &format!("--- page {} ---\n", p + 1);
        for (i, line) in page.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mark = match layout {
                Some(l) if !stopped && l.stop.as_ref().is_some_and(|re| re.is_match(line)) => {
                    stopped = true;
                    "■".to_string()
                }
                Some(l) if !stopped => match transactions_in_line(l, line).count() {
                    0 => String::new(),
                    1 => "✓".to_string(),
                    n => format!("✓{n}"),
                },
                _ => String::new(),
            };
            let shape = (mark, mask(line));
            if previous.as_ref() != Some(&shape) {
                flush(&mut out, &previous, &mut run);
                previous = Some(shape);
            }
            run.push(i + 1);
        }
    }
    flush(&mut out, &previous, &mut run);
    out
}

/// Letters -> a/A (keeping case), digits -> 9, long runs of spaces shortened;
/// punctuation, "R$" and minus signs stay.
fn mask(line: &str) -> String {
    let mut out = String::new();
    let mut spaces = 0;
    let chars: Vec<char> = line.trim_end().chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            spaces += 1;
            i += 1;
            continue;
        }
        if spaces > 0 {
            out.push_str(&" ".repeat(spaces.min(3)));
            spaces = 0;
        }
        if c == 'R' && chars.get(i + 1) == Some(&'$') {
            out += "R$";
            i += 2;
            continue;
        }
        out.push(if c.is_alphabetic() {
            if c.is_uppercase() {
                'A'
            } else {
                'a'
            }
        } else if c.is_numeric() {
            '9'
        } else {
            c
        });
        i += 1;
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn layouts() -> Layouts {
        Layouts::parse(include_str!("../parsers.toml")).unwrap()
    }

    fn layout(name: &str) -> Layout {
        layouts().0.into_iter().find(|l| l.name == name).unwrap()
    }

    /// A made-up checking statement, laid out like `pdftotext -layout` prints one.
    pub const CHECKING: &str = "\
ITAU UNIBANCO                                   Extrato conta corrente
Agência 0001   Conta 12345-6                    Período: 01/10/2026 a 31/10/2026

data          lançamentos                              valor (R$)        saldo (R$)
30/09/2026    SALDO ANTERIOR                                                1.000,00
01/10/2026    PAGTO SALARIO                              6.200,00
02/10/2026    PIX TRANSF JOAO S02/10                      -150,00
02/10/2026    CAFE DA ESQUINA                                -5,00
02/10/2026    CAFE DA ESQUINA                                -5,00
02/10/2026    SALDO DO DIA                                                  7.040,00
05/10/2026    TAR PACOTE ITAU                               -39,90 D
06/10/2026    ESTORNO TARIFA                                 39,90 C
lançamentos futuros
10/11/2026    AGENDAMENTO ALUGUEL                         -2.500,00
";

    /// A made-up card bill: two columns, an installment, a payment, future installments.
    pub const CARD: &str = "\
Itaú Personnalité Visa Platinum        cartão final 4321
Vencimento: 05/01/2027        Fatura fechada em 28/12/2026        Pagamento mínimo   150,00
Lançamentos: compras e saques
DATA   ESTABELECIMENTO                  VALOR EM R$      DATA   ESTABELECIMENTO              VALOR EM R$
15/12  PAGAMENTO EFETUADO                   -950,00      18/12  IFOOD *RESTAURANTE                58,90
20/12  MAGAZINE LUIZA 02/10                 150,00       28/12  UBER *TRIP                        22,50
02/01  NETFLIX.COM                           55,90
       Total dos lançamentos atuais       1.287,30
Compras parceladas - próximas faturas
20/12  MAGAZINE LUIZA 03/10                 150,00
\x0c";

    #[test]
    fn the_projects_parsers_file_is_valid() {
        assert_eq!(layouts().0.len(), 2);
    }

    #[test]
    fn detects_the_layout_from_keywords() {
        assert_eq!(layouts().detect(CHECKING).unwrap().name, "itau_checking");
        assert_eq!(layouts().detect(CARD).unwrap().name, "itau_card");
        assert!(layouts().detect("nothing to see").is_none());
    }

    #[test]
    fn reads_a_checking_statement() {
        let s = parse(CHECKING, &layout("itau_checking"));
        assert_eq!(s.account, "12345-6");
        assert_eq!(s.account_type, AccountType::Checking);
        let rows: Vec<(&str, i64, &str)> =
            s.transactions.iter().map(|t| (t.date.as_str(), t.amount.0, t.memo.as_str())).collect();
        assert_eq!(
            rows,
            vec![
                ("2026-10-01", 620_000, "PAGTO SALARIO"),
                ("2026-10-02", -15_000, "PIX TRANSF JOAO S02/10"),
                ("2026-10-02", -500, "CAFE DA ESQUINA"),
                ("2026-10-02", -500, "CAFE DA ESQUINA"),
                ("2026-10-05", -3990, "TAR PACOTE ITAU"),
                ("2026-10-06", 3990, "ESTORNO TARIFA"),
            ]
        );
        // Two identical coffees stay two transactions, and get stable, different ids.
        assert_ne!(s.transactions[2].id, s.transactions[3].id);
        assert_eq!(s.transactions[2].id, parse(CHECKING, &layout("itau_checking")).transactions[2].id);
    }

    #[test]
    fn reads_a_card_bill_with_two_columns_and_the_year_from_the_due_date() {
        let s = parse(CARD, &layout("itau_card"));
        assert_eq!(s.account, "4321");
        assert_eq!(s.account_type, AccountType::CreditCard);
        let rows: Vec<(&str, i64, &str)> =
            s.transactions.iter().map(|t| (t.date.as_str(), t.amount.0, t.memo.as_str())).collect();
        assert_eq!(
            rows,
            vec![
                ("2026-12-15", 95_000, "PAGAMENTO EFETUADO"), // a payment is money in on the card
                ("2026-12-18", -5890, "IFOOD *RESTAURANTE"),
                ("2026-12-20", -15_000, "MAGAZINE LUIZA 02/10"),
                ("2026-12-28", -2250, "UBER *TRIP"),
                ("2027-01-02", -5590, "NETFLIX.COM"), // after the December rows, but January: 2027
            ]
        );
    }

    #[test]
    fn amounts_with_signs_and_marks() {
        assert_eq!(parse_amount("-1.234,56", None), Some(Money(-123_456)));
        assert_eq!(parse_amount("1.234,56-", None), Some(Money(-123_456)));
        assert_eq!(parse_amount("R$ 12,00", None), Some(Money(1200)));
        assert_eq!(parse_amount("39,90", Some("D")), Some(Money(-3990)));
        assert_eq!(parse_amount("39,90", Some("c")), Some(Money(3990)));
        assert_eq!(full_date("05/10/26", None).as_deref(), Some("2026-10-05"));
        assert_eq!(full_date("05/10", None), None);
        assert_eq!(full_date("32/10/2026", None), None);
    }

    #[test]
    fn inspect_shows_no_letters_or_digits_from_the_file() {
        let secret = "SEGREDO FULANODETAL 98765-4 CPF 123.456.789-00";
        let text = format!("{CHECKING}{secret}\n");
        let report = inspect(&text, &layouts());
        for word in ["SEGREDO", "FULANODETAL", "98765", "123.456", "SALARIO", "6.200", "12345-6", "ESQUINA"] {
            assert!(!report.contains(word), "{word} leaked:\n{report}");
        }
        // Every shape line holds only a/A/9, spaces, punctuation and "R$".
        for line in report.lines().filter(|l| l.starts_with('✓') || l.starts_with("  ")) {
            let shape = line.split_once("  ").map_or("", |(_, rest)| rest).trim_start();
            let shape = shape.split_once("  ").map_or(shape, |(_, s)| s);
            assert!(
                shape
                    .replace("R$", "")
                    .chars()
                    .all(|c| matches!(c, 'a' | 'A' | '9' | ' ') || !c.is_alphanumeric()),
                "{line}"
            );
        }
        assert!(report.contains("Looks like: itau_checking"));
        assert!(report.contains("Transactions found: 6"));
        assert!(report.contains("99/99/9999   AAAAA AAAAAAA   9.999,99"), "{report}");
        assert!(report.contains("(2x)"), "the two coffee lines collapse into one:\n{report}");
    }

    #[test]
    fn inspect_marks_what_the_importer_reads() {
        let report = inspect(CARD, &layouts());
        let marked = |mark: &str| report.lines().filter(|l| l.starts_with(mark)).count();
        assert_eq!(marked("✓2"), 2, "two lines with two transactions each:\n{report}");
        assert_eq!(marked("■"), 1, "{report}");
        // The due-date line holds a date and an amount but is skipped ("Pagamento mínimo").
        let due = report.lines().find(|l| l.contains("Aaaaaaaaaa: 99/99/9999")).unwrap();
        assert!(!due.starts_with('✓'), "{report}");
        // The installment after "próximas faturas" isn't read.
        assert!(!report.lines().last().unwrap().starts_with('✓'), "{report}");
        assert!(report.contains("Transactions found: 5"));
    }

    /// A one-page PDF with `lines` in a fixed-width font, so `pdftotext -layout`
    /// gives the columns back. Used to test with real (made-up) PDF files.
    pub fn make_pdf(lines: &[&str]) -> Vec<u8> {
        let mut content = String::from("BT /F1 8 Tf 10 TL 20 820 Td\n");
        for line in lines {
            let mut escaped = String::new();
            for b in encoding_rs::WINDOWS_1252.encode(line).0.iter() {
                match b {
                    b'(' | b')' | b'\\' => escaped += &format!("\\{}", *b as char),
                    0x20..=0x7e => escaped.push(*b as char),
                    _ => escaped += &format!("\\{b:03o}"),
                }
            }
            content += &format!("({escaped}) Tj T*\n");
        }
        content += "ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>".to_string(),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf += &format!("{} 0 obj\n{object}\nendobj\n", i + 1);
        }
        let xref = pdf.len();
        pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
        for offset in offsets {
            pdf += &format!("{offset:010} 00000 n \n");
        }
        pdf += &format!("trailer << /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
        pdf.into_bytes()
    }

    #[test]
    fn reads_a_real_pdf_file() {
        if Command::new("pdftotext").arg("-v").output().is_err() {
            eprintln!("pdftotext not installed; skipping (it is in the Docker image)");
            return;
        }
        let path = std::env::temp_dir().join(format!("finance-test-{}.pdf", std::process::id()));
        std::fs::write(&path, make_pdf(&CARD.trim_end_matches('\x0c').lines().collect::<Vec<_>>())).unwrap();
        let text = extract_text(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let layouts = layouts();
        let layout = layouts.detect(&text).expect("detected");
        assert_eq!(layout.name, "itau_card");
        let s = parse(&text, layout);
        assert_eq!(s.transactions.len(), 5, "{text}");
        assert_eq!(s.transactions[1].memo, "IFOOD *RESTAURANTE");
    }
}
