//! Reading PDF statements (Itaú checking statements and card bills).
//!
//! A PDF has no transaction structure like OFX, only pieces of text placed on a
//! page. `pdftohtml -xml` (poppler, installed in the Docker image) lists each piece
//! with its position and color. We put the pieces back into lines that keep the
//! page's columns ([`Doc`]), then find transactions in those lines with the regular
//! expressions in parsers.toml, so a layout can be adjusted without rebuilding.
//!
//! Colors matter: Itaú prints money in green and money out in red on checking
//! statements, sometimes without a minus sign. With `color_sign` the color decides.
//!
//! Every run also produces checks that need no one to look at the data: the
//! checking statement's daily balances must add up, and the card bill's purchases
//! must add up to its totals.
//!
//! `inspect` prints the *shape* of each line (letters become a/A, digits 9) so a
//! layout can be fixed from someone's statement without anyone seeing its content.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::events::AccountType;
use crate::money::Money;
use crate::ofx::{transaction_id, ParsedTransaction, Statement};

/// A pt-BR amount as statements print it: "1.234,56", "-150,00", "R$ 12,00", "39,90-".
const AMOUNT: &str = r"-?\s?(?:R\$\s?)?\d{1,3}(?:\.\d{3})*,\d{2}-?";

fn amount_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(AMOUNT).expect("valid pattern"))
}

// --- parsers.toml --------------------------------------------------------------

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
    #[serde(default)]
    color_sign: bool,
    balance: Option<String>,
    #[serde(default)]
    totals: Vec<String>,
    #[serde(default)]
    fees: Vec<FeeDef>,
    last_amount_from: Option<String>,
    last_amount_until: Option<String>,
    bank_category: Option<String>,
}

#[derive(Deserialize)]
struct FeeDef {
    pattern: String,
    memo: String,
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
    color_sign: bool,
    balance: Option<Regex>,
    totals: Vec<Regex>,
    fees: Vec<(Regex, String)>,
    last_amount_from: Option<Regex>,
    last_amount_until: Option<Regex>,
    bank_category: Option<Regex>,
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
            // Case matters here: Itaú prints its category in lowercase, the city in capitals.
            let bank_category = def
                .bank_category
                .as_deref()
                .map(|p| Regex::new(p).with_context(|| format!("[pdf.{name}] bad `bank_category` pattern")))
                .transpose()?;
            if bank_category.as_ref().is_some_and(|re| !re.capture_names().any(|n| n == Some("category"))) {
                bail!("[pdf.{name}] `bank_category` needs a (?P<category>...) group");
            }
            layouts.push(Layout {
                account: optional(&def.account, "account")?,
                skip: optional(&def.skip, "skip")?,
                stop: optional(&def.stop, "stop")?,
                balance: optional(&def.balance, "balance")?,
                last_amount_from: optional(&def.last_amount_from, "last_amount_from")?,
                last_amount_until: optional(&def.last_amount_until, "last_amount_until")?,
                totals: def.totals.iter().map(|p| pattern(p, &name, "totals")).collect::<Result<_>>()?,
                fees: def
                    .fees
                    .iter()
                    .map(|f| Ok((pattern(&f.pattern, &name, "fees")?, f.memo.clone())))
                    .collect::<Result<_>>()?,
                detect: def.detect.iter().map(|k| k.to_uppercase()).collect(),
                account_type: def.account_type,
                purchases_positive: def.purchases_positive,
                color_sign: def.color_sign,
                bank_category,
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

// --- The document: lines of text with their colors --------------------------------

/// The color of a piece of text, as far as money is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Green,
    Red,
}

impl Tone {
    /// "#1a9933" -> Green. Only clearly green or red colors count.
    fn of_hex(hex: &str) -> Tone {
        let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
        let (r, g, b) = (((v >> 16) & 255) as i32, ((v >> 8) & 255) as i32, (v & 255) as i32);
        if g > r + 40 && g > b + 20 {
            Tone::Green
        } else if r > g + 40 && r > b + 40 {
            Tone::Red
        } else {
            Tone::Neutral
        }
    }
}

/// One line of a page, with the byte ranges that are printed in green or red.
pub struct Row {
    pub text: String,
    tones: Vec<(usize, usize, Tone)>,
}

impl Row {
    fn plain(text: &str) -> Row {
        Row { text: text.to_string(), tones: Vec::new() }
    }

    /// The color of the text at `start..end` (bytes): the first green or red part, else neutral.
    fn tone(&self, start: usize, end: usize) -> Tone {
        self.tones.iter().find(|(s, e, _)| *s < end && *e > start).map_or(Tone::Neutral, |t| t.2)
    }
}

/// A statement's pages, each a list of lines.
pub struct Doc {
    pub pages: Vec<Vec<Row>>,
}

/// Where a line is: page and line number, both from 1.
type LineRef = (usize, usize);

fn line_ref((page, line): LineRef) -> String {
    format!("{page}:{line}")
}

impl Doc {
    /// Plain text, pages separated by form feeds ("\x0c"). No colors.
    pub fn from_text(text: &str) -> Doc {
        Doc { pages: text.split('\x0c').map(|page| page.lines().map(Row::plain).collect()).collect() }
    }

    /// The XML that `pdftohtml -xml` prints: pieces of text with position and font
    /// (which carries the color), grouped into lines that keep the columns.
    pub fn from_xml(xml: &str) -> Doc {
        let page_re = Regex::new(r"(?s)<page\b[^>]*>(.*?)</page>").expect("valid pattern");
        let font_re =
            Regex::new(r##"<fontspec\s+id="(\d+)"[^>]*?color="#([0-9a-fA-F]{6})""##).expect("valid pattern");
        let tones: HashMap<String, Tone> =
            font_re.captures_iter(xml).map(|c| (c[1].to_string(), Tone::of_hex(&c[2]))).collect();
        Doc { pages: page_re.captures_iter(xml).map(|page| rows_of_page(&page[1], &tones)).collect() }
    }

    /// The whole text, for keyword detection and header fields.
    pub fn text(&self) -> String {
        self.pages
            .iter()
            .map(|page| page.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>()
            .join("\n\x0c")
    }
}

struct Piece {
    top: i32,
    left: i32,
    width: i32,
    text: String,
    tone: Tone,
}

fn rows_of_page(page_xml: &str, tones: &HashMap<String, Tone>) -> Vec<Row> {
    let text_re = Regex::new(
        r#"(?s)<text\s+top="(-?\d+)"\s+left="(-?\d+)"\s+width="(\d+)"\s+height="\d+"\s+font="(\d+)"[^>]*>(.*?)</text>"#,
    )
    .expect("valid pattern");
    let mut pieces: Vec<Piece> = text_re
        .captures_iter(page_xml)
        .filter_map(|c| {
            let text = decode_xml(&c[5]);
            if text.trim().is_empty() {
                return None;
            }
            Some(Piece {
                top: c[1].parse().ok()?,
                left: c[2].parse().ok()?,
                width: c[3].parse().ok()?,
                text,
                tone: tones.get(&c[4]).copied().unwrap_or(Tone::Neutral),
            })
        })
        .collect();
    if pieces.is_empty() {
        return Vec::new();
    }
    // The page's average character width turns pixel gaps into spaces.
    let (width, chars) = pieces
        .iter()
        .filter(|p| p.text.chars().count() >= 3)
        .fold((0.0, 0), |(w, n), p| (w + p.width as f64, n + p.text.chars().count()));
    let char_width = if chars > 0 { width / chars as f64 } else { 6.0 };
    let page_left = pieces.iter().map(|p| p.left).min().unwrap_or(0);

    pieces.sort_by_key(|p| (p.top, p.left));
    let mut rows: Vec<Vec<Piece>> = Vec::new();
    for piece in pieces {
        match rows.last_mut() {
            Some(row) if (piece.top - row[0].top).abs() <= 3 => row.push(piece),
            _ => rows.push(vec![piece]),
        }
    }
    rows.into_iter()
        .map(|mut pieces| {
            pieces.sort_by_key(|p| p.left);
            let mut row = Row { text: String::new(), tones: Vec::new() };
            let mut right = page_left as f64;
            for p in pieces {
                let gap = (p.left as f64 - right) / char_width;
                let spaces = if row.text.is_empty() {
                    gap.round().max(0.0) as usize
                } else if gap < 0.3 {
                    0 // pieces of one word
                } else {
                    (gap.round() as usize).max(1)
                };
                row.text.push_str(&" ".repeat(spaces));
                let start = row.text.len();
                row.text.push_str(&p.text);
                if p.tone != Tone::Neutral {
                    row.tones.push((start, row.text.len(), p.tone));
                }
                right = (p.left + p.width) as f64;
            }
            row
        })
        .collect()
}

fn decode_xml(text: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    static NUMERIC: OnceLock<Regex> = OnceLock::new();
    let text = TAG.get_or_init(|| Regex::new(r"<[^>]*>").expect("valid pattern")).replace_all(text, "");
    let text = NUMERIC.get_or_init(|| Regex::new(r"&#(\d+);").expect("valid pattern")).replace_all(
        &text,
        |c: &regex::Captures| {
            c[1].parse::<u32>().ok().and_then(char::from_u32).map(String::from).unwrap_or_default()
        },
    );
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The PDF's lines, with colors.
pub fn extract(path: &Path) -> Result<Doc> {
    let output = Command::new("pdftohtml")
        .args(["-xml", "-i", "-stdout", "-q", "-nodrm"])
        .arg(path)
        .output()
        .context("running pdftohtml (it's installed in the Docker image: use ./finance)")?;
    if !output.status.success() {
        bail!("pdftohtml failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(Doc::from_xml(&String::from_utf8_lossy(&output.stdout)))
}

// --- Reading transactions -----------------------------------------------------------

/// What a layout makes of one possible transaction in a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Read,
    /// A balance line (checking): used for the balance check, not a transaction.
    Balance,
    /// Matched `skip` (totals, minimum payment...).
    Skipped,
    /// The date comes right after a "Label:" (Vencimento: 05/11/2026 ...).
    AfterLabel,
}

struct Candidate {
    start: usize,
    date: String,
    memo: String,
    amount: (usize, usize),
    debit_credit: Option<String>,
    /// Where the description starts, in characters: a category line below lines up with it.
    description_column: usize,
    verdict: Verdict,
}

fn candidates(layout: &Layout, text: &str) -> Vec<Candidate> {
    layout
        .transaction
        .captures_iter(text)
        .map(|caps| {
            let (date, description, amount) =
                (&caps["date"], caps.name("description").unwrap(), caps.name("amount").unwrap());
            let date_start = caps.name("date").unwrap().start();
            let memo = collapse_spaces(description.as_str());
            let verdict = if layout.balance.as_ref().is_some_and(|re| re.is_match(&memo)) {
                Verdict::Balance
            } else if layout.skip.as_ref().is_some_and(|re| re.is_match(&memo)) {
                Verdict::Skipped
            } else if text[..date_start].trim_end().ends_with(':') {
                Verdict::AfterLabel
            } else {
                Verdict::Read
            };
            Candidate {
                start: caps.get(0).unwrap().start(),
                date: date.to_string(),
                memo,
                amount: (amount.start(), amount.end()),
                debit_credit: caps.name("dc").map(|m| m.as_str().to_string()),
                description_column: text[..description.start()].chars().count(),
                verdict,
            }
        })
        .collect()
}

/// In a section with two amount columns (US$ then R$), use the last amount before
/// the next transaction in the line.
fn use_last_amounts(text: &str, candidates: &mut [Candidate]) {
    for i in 0..candidates.len() {
        let until = candidates.get(i + 1).map_or(text.len(), |next| next.start);
        let from = candidates[i].amount.0;
        if let Some(last) = amount_re().find_iter(&text[from..until]).last() {
            candidates[i].amount = (from + last.start(), from + last.end());
        }
    }
}

fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The everything a layout read from a statement, plus checks and a mark per line
/// for `inspect` (in the same page/line structure as the [`Doc`]).
pub struct Analysis {
    pub statement: Statement,
    /// Results of the balance / bill checks and any color conflicts. Dates, line
    /// numbers and differences only, never descriptions.
    pub checks: Vec<String>,
    marks: Vec<Vec<String>>,
}

struct Balance {
    date: String,
    amount: Money,
    order: usize,
}

/// Reads a statement with `layout`.
pub fn parse(doc: &Doc, layout: &Layout) -> Analysis {
    let full_text = doc.text();
    let reference = latest_full_date(&full_text);
    let account = layout
        .account
        .as_ref()
        .and_then(|re| re.captures(&full_text))
        .and_then(|c| c.name("account").map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| layout.name.clone());

    let mut transactions: Vec<ParsedTransaction> = Vec::new();
    let mut orders: Vec<usize> = Vec::new(); // the line order of each transaction, for the balance check
                                             // PDFs have no transaction ids, so identical rows (two R$ 5,00 coffees on one
                                             // day) are told apart by how many came before them in the file.
    let mut seen: HashMap<(String, i64, String), usize> = HashMap::new();
    let mut balances: Vec<Balance> = Vec::new();
    let (mut bill_total, mut totals_found) = (Money::ZERO, 0);
    let mut conflicts: Vec<String> = Vec::new();
    let mut awaiting_category: Vec<(usize, usize)> = Vec::new(); // (transaction, description column)
    let (mut stopped, mut last_amount_section) = (false, false);
    let mut marks: Vec<Vec<String>> = Vec::new();
    let mut order = 0;

    let mut push = |transactions: &mut Vec<ParsedTransaction>,
                    orders: &mut Vec<usize>,
                    date: String,
                    amount: Money,
                    memo: String,
                    order: usize| {
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
            bank_category: None,
        });
        orders.push(order);
    };

    for (p, page) in doc.pages.iter().enumerate() {
        let mut page_marks = Vec::new();
        for (r, row) in page.iter().enumerate() {
            order += 1;
            let at = (p + 1, r + 1);
            let text = row.text.as_str();
            if stopped || text.trim().is_empty() {
                page_marks.push(String::new());
                continue;
            }
            if layout.stop.as_ref().is_some_and(|re| re.is_match(text)) {
                stopped = true;
                page_marks.push("■".to_string());
                continue;
            }
            // A category line under the previous line's purchases?
            let waiting = std::mem::take(&mut awaiting_category);
            if let (false, Some(re)) = (waiting.is_empty(), &layout.bank_category) {
                let mut found = false;
                for (tx, column) in waiting {
                    if let Some(category) = category_at(text, column, re) {
                        transactions[tx].bank_category = Some(category);
                        found = true;
                    }
                }
                if found {
                    page_marks.push("c".to_string());
                    continue;
                }
            }
            if layout.last_amount_from.as_ref().is_some_and(|re| re.is_match(text)) {
                last_amount_section = true;
            }
            if layout.last_amount_until.as_ref().is_some_and(|re| re.is_match(text)) {
                last_amount_section = false;
            }

            let mut mark = String::new();
            if let Some(total) = layout.totals.iter().find_map(|re| amount_after(re, text)) {
                bill_total = bill_total + Money(total.0.abs());
                totals_found += 1;
                mark.push('Σ');
            }
            for (re, memo) in &layout.fees {
                if let (Some(amount), Some(date)) =
                    (amount_after(re, text), transactions.last().map(|t| t.date.clone()))
                {
                    let amount = if layout.purchases_positive { Money(-amount.0) } else { amount };
                    push(&mut transactions, &mut orders, date, amount, memo.clone(), order);
                    mark.push('i');
                }
            }

            let mut found = candidates(layout, text);
            if last_amount_section {
                use_last_amounts(text, &mut found);
            }
            let (mut read, mut tone_marks, mut rejected) = (0, String::new(), None);
            for c in &found {
                let date = full_date(&c.date, reference);
                match (c.verdict, date) {
                    (Verdict::Balance, Some(date)) => {
                        if let Some((amount, _, _)) = signed_amount(layout, row, c) {
                            balances.push(Balance { date, amount, order });
                            mark.push('=');
                        }
                    }
                    (Verdict::Read, Some(date)) => {
                        let Some((amount, tone, conflict)) = signed_amount(layout, row, c) else { continue };
                        if conflict {
                            conflicts.push(line_ref(at));
                        }
                        let amount = if layout.purchases_positive { Money(-amount.0) } else { amount };
                        push(&mut transactions, &mut orders, date, amount, c.memo.clone(), order);
                        awaiting_category.push((transactions.len() - 1, c.description_column));
                        read += 1;
                        tone_marks.push(match tone {
                            Tone::Green => '+',
                            Tone::Red => '−',
                            Tone::Neutral => '\0',
                        });
                    }
                    (Verdict::Read, None) => rejected = rejected.or(Some("?d")),
                    (Verdict::Skipped, _) => rejected = rejected.or(Some("?s")),
                    (Verdict::AfterLabel, _) => rejected = rejected.or(Some("?l")),
                    (Verdict::Balance, None) => {}
                }
            }
            let tone_marks: String = tone_marks.chars().filter(|c| *c != '\0').collect();
            let mark = match (read, rejected) {
                (0, Some(reason)) if mark.is_empty() => reason.to_string(),
                (0, _) => mark,
                (1, _) => format!("✓{tone_marks}{mark}"),
                (n, _) => format!("✓{n}{tone_marks}{mark}"),
            };
            page_marks.push(mark);
        }
        marks.push(page_marks);
    }

    let mut checks = Vec::new();
    if layout.balance.is_some() {
        checks.extend(balance_check(&balances, &transactions, &orders));
    }
    if !layout.totals.is_empty() {
        checks.push(bill_check(&transactions, bill_total, totals_found));
    }
    if !conflicts.is_empty() {
        checks.push(format!(
            "Color and minus sign disagree on line{} {} (the color was used).",
            if conflicts.len() == 1 { "" } else { "s" },
            conflicts.join(", ")
        ));
    }
    Analysis {
        statement: Statement { account, account_type: layout.account_type, transactions },
        checks,
        marks,
    }
}

/// The amount with its sign: from the color when `color_sign` and the amount is
/// green or red, else from a minus sign or a D/C mark. Also says whether a printed
/// minus disagreed with the color.
fn signed_amount(layout: &Layout, row: &Row, c: &Candidate) -> Option<(Money, Tone, bool)> {
    let amount = parse_amount(&row.text[c.amount.0..c.amount.1], c.debit_credit.as_deref())?;
    let tone = row.tone(c.amount.0, c.amount.1);
    if !layout.color_sign {
        return Some((amount, tone, false));
    }
    Some(match tone {
        Tone::Red => (Money(-amount.0.abs()), tone, false),
        Tone::Green => (Money(amount.0.abs()), tone, amount.0 < 0),
        Tone::Neutral => (amount, tone, false),
    })
}

/// The first amount after `label` in `text`.
fn amount_after(label: &Regex, text: &str) -> Option<Money> {
    let m = label.find(text)?;
    let amount = amount_re().find(&text[m.end()..])?;
    parse_amount(amount.as_str(), None)
}

/// The bank's category on a line under a purchase, lined up with its description.
fn category_at(text: &str, column: usize, re: &Regex) -> Option<String> {
    static SEGMENT: OnceLock<Regex> = OnceLock::new();
    let segment = SEGMENT.get_or_init(|| Regex::new(r"\S+(?: \S+)*").expect("valid pattern"));
    segment.find_iter(text).find_map(|m| {
        let start = text[..m.start()].chars().count();
        if start.abs_diff(column) > 3 {
            return None;
        }
        let category = re.captures(m.as_str())?.name("category")?.as_str().trim().to_string();
        (!category.is_empty()).then_some(category)
    })
}

/// Checking: each balance minus the one before must equal the movements between them.
fn balance_check(balances: &[Balance], transactions: &[ParsedTransaction], orders: &[usize]) -> Vec<String> {
    if balances.len() < 2 {
        return vec!["Balance check: not enough balance lines (SALDO) to check.".to_string()];
    }
    let moved = |from: usize, to: usize| -> Money {
        transactions
            .iter()
            .zip(orders)
            .filter(|(_, o)| **o > from && **o <= to)
            .fold(Money::ZERO, |s, (t, _)| s + t.amount)
    };
    let (mut good, mut off, mut flipped) = (0, Vec::new(), 0);
    for pair in balances.windows(2) {
        let (movements, expected) = (moved(pair[0].order, pair[1].order), pair[1].amount - pair[0].amount);
        if movements == expected {
            good += 1;
        } else {
            if movements.0 == -expected.0 {
                flipped += 1;
            }
            off.push(format!("  {} off by R$ {}", pair[1].date, Money((movements - expected).0.abs())));
        }
    }
    let total = balances.len() - 1;
    let mut lines = vec![format!(
        "Balance check: {good} of {total} balances add up{}",
        if off.is_empty() { "." } else { ":" }
    )];
    lines.extend(off.iter().take(15).cloned());
    if off.len() > 15 {
        lines.push(format!("  ...and {} more", off.len() - 15));
    }
    if flipped > 0 && flipped == off.len() {
        lines.push(
            "  Where it's off, the movements are exactly reversed: the signs look flipped.".to_string(),
        );
    }
    let (first, last) = (&balances[0], &balances[balances.len() - 1]);
    let whole = moved(first.order, last.order) - (last.amount - first.amount);
    lines.push(if whole == Money::ZERO {
        "Whole period: the movements match closing minus opening balance.".to_string()
    } else {
        format!(
            "Whole period: the movements are off by R$ {} from closing minus opening balance.",
            Money(whole.0.abs())
        )
    });
    lines
}

/// Card: the purchases read (money out) must add up to the bill's totals.
fn bill_check(transactions: &[ParsedTransaction], bill_total: Money, totals_found: usize) -> String {
    if totals_found == 0 {
        return "Bill check: no total lines found to compare with.".to_string();
    }
    let purchases =
        transactions.iter().filter(|t| t.amount.0 < 0).fold(Money::ZERO, |s, t| s + Money(-t.amount.0));
    if purchases == bill_total {
        "Bill check: OK, the purchases add up to the bill's totals.".to_string()
    } else {
        format!(
            "Bill check: the purchases differ from the bill's totals by R$ {}.",
            Money((purchases - bill_total).0.abs())
        )
    }
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

const LEGEND: &str = "\
Line shapes only: letters became a/A, digits 9. Nothing from the file itself is shown.
Marks: ✓ transaction (✓2 = two in the line; + green, − red amount)   c category line
       = balance line   Σ bill total   i fee   ■ reading stops here
       ?s / ?l / ?d looks like a transaction but was skipped (skip pattern / after a
       \"Label:\" / bad date)
";

/// A report on a statement that shows no content: each line's shape and what the
/// detected layout does with it, the keyword counts, the transaction count and the
/// checks.
pub fn inspect(doc: &Doc, layouts: &Layouts) -> String {
    let mut out = String::from(LEGEND);
    let full_text = doc.text();
    let hits = layouts.keyword_hits(&full_text);
    let counts: Vec<String> = hits.iter().map(|(l, n)| format!("{} {n}", l.name)).collect();
    let layout = layouts.detect(&full_text);
    out += &format!(
        "Looks like: {} (keyword hits: {})\n",
        layout.map_or("nothing known", |l| l.name.as_str()),
        counts.join(", ")
    );
    let analysis = layout.map(|l| parse(doc, l));
    if let Some(analysis) = &analysis {
        out += &format!("Transactions found: {}\n", analysis.statement.transactions.len());
        for check in &analysis.checks {
            out += check;
            out.push('\n');
        }
    }

    // Consecutive lines with the same mark and shape are shown once, with a count.
    let mut previous: Option<(String, String)> = None;
    let mut run: Vec<usize> = Vec::new();
    let mut page_no = 0;
    let flush = |out: &mut String, previous: &Option<(String, String)>, run: &mut Vec<usize>, page: usize| {
        if let (Some((mark, shape)), Some(first)) = (previous, run.first()) {
            let lines = if run.len() == 1 {
                format!("{page}:{first}")
            } else {
                format!("{page}:{first}-{} ({}x)", run[run.len() - 1], run.len())
            };
            *out += &format!("{mark:<6} {lines:>14}  {shape}\n");
        }
        run.clear();
    };
    for (p, page) in doc.pages.iter().enumerate() {
        if page.iter().all(|r| r.text.trim().is_empty()) {
            continue;
        }
        flush(&mut out, &previous, &mut run, page_no);
        previous = None;
        page_no = p + 1;
        out += &format!("--- page {page_no} ---\n");
        for (i, row) in page.iter().enumerate() {
            if row.text.trim().is_empty() {
                continue;
            }
            let mark = analysis.as_ref().and_then(|a| a.marks.get(p)?.get(i).cloned()).unwrap_or_default();
            let shape = (mark, mask(&row.text));
            if previous.as_ref() != Some(&shape) {
                flush(&mut out, &previous, &mut run, page_no);
                previous = Some(shape);
            }
            run.push(i + 1);
        }
    }
    flush(&mut out, &previous, &mut run, page_no);
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

    fn rows(statement: &Statement) -> Vec<(&str, i64, &str)> {
        statement.transactions.iter().map(|t| (t.date.as_str(), t.amount.0, t.memo.as_str())).collect()
    }

    /// A made-up checking statement, laid out as the PDF reader rebuilds one: a
    /// valor column and a saldo column, balances, a D/C-marked pair, future items.
    pub const CHECKING: &str = "\
ITAU UNIBANCO                                   Extrato conta corrente
Agência 0001   Conta 12345-6                    Período: 01/10/2026 a 31/10/2026
data          lançamentos                              valor (R$)        saldo (R$)
30/09/2026    SALDO ANTERIOR                                                1.000,00
01/10/2026    PAGTO SALARIO                              6.200,00
01/10/2026    SALDO DO DIA                                                  7.200,00
02/10/2026    PIX TRANSF JOAO S02/10                      -150,00
02/10/2026    CAFE DA ESQUINA                                -5,00
02/10/2026    CAFE DA ESQUINA                                -5,00          7.040,00
02/10/2026    SALDO DO DIA                                                  7.040,00
05/10/2026    TAR PACOTE ITAU                               -39,90 D
06/10/2026    ESTORNO TARIFA                                 39,90 C
06/10/2026    SALDO DO DIA                                                  7.040,00
lançamentos futuros
10/11/2026    AGENDAMENTO ALUGUEL                         -2.500,00
";

    /// Two columns of a card bill, 52 characters apart.
    fn two(left: &str, right: &str) -> String {
        format!("{left:<52}{right}")
    }

    /// A made-up card bill with everything the real ones were seen to have.
    pub fn card() -> String {
        [
            "Itaú Personnalité Visa Platinum        cartão final 4321".to_string(),
            "Vencimento: 05/11/2026   = Total desta fatura   1.153,80".to_string(),
            "Lançamentos: compras e saques   12/10  IFOOD *RESTAURANTE   58,90".to_string(),
            format!("{}restaurante SAO PAULO", " ".repeat(39)),
            two("DATA   ESTABELECIMENTO       VALOR EM R$", "DATA   ESTABELECIMENTO       VALOR EM R$"),
            two("10/10  PAGAMENTO EFETUADO        -950,00", "14/10  MAGAZINE LUIZA 02/10      150,00"),
            two("", "       vestuário SAO PAULO"),
            two("18/10  UBER *TRIP                 22,50", "Total para próximas faturas     300,00"),
            two("       transporte SAO PAULO", ""),
            two("20/10  COCO BAMBU                120,00", "22/10  NETFLIX.COM                55,90"),
            two("       restaurante SAO PAULO", "       serviços SAO PAULO"),
            "24/10   SUPERMERCADO BOMPRECO   210,00".to_string(),
            "        supermercado RECIFE".to_string(),
            "       Total dos lançamentos atuais        617,30".to_string(),
            "DATA   ESTABELECIMENTO                 US$       R$".to_string(),
            "25/10  STEAMGAMES.COM WA USA        10,00     55,40".to_string(),
            "       Total lançamentos inter. em R$      55,40".to_string(),
            "       Repasse de IOF em R$                 1,94".to_string(),
            "       Total transações inter. em R$       57,34".to_string(),
            "Compras parceladas - próximas faturas".to_string(),
            "14/10  MAGAZINE LUIZA 03/10          150,00".to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn the_projects_parsers_file_is_valid() {
        assert_eq!(layouts().0.len(), 2);
    }

    #[test]
    fn detects_the_layout_from_keywords() {
        assert_eq!(layouts().detect(CHECKING).unwrap().name, "itau_checking");
        assert_eq!(layouts().detect(&card()).unwrap().name, "itau_card");
        assert!(layouts().detect("nothing to see").is_none());
    }

    #[test]
    fn reads_a_checking_statement_and_its_balances_add_up() {
        let a = parse(&Doc::from_text(CHECKING), &layout("itau_checking"));
        assert_eq!(a.statement.account, "12345-6");
        assert_eq!(a.statement.account_type, AccountType::Checking);
        assert_eq!(
            rows(&a.statement),
            vec![
                ("2026-10-01", 620_000, "PAGTO SALARIO"),
                ("2026-10-02", -15_000, "PIX TRANSF JOAO S02/10"),
                ("2026-10-02", -500, "CAFE DA ESQUINA"),
                ("2026-10-02", -500, "CAFE DA ESQUINA"), // the saldo column next to it isn't the amount
                ("2026-10-05", -3990, "TAR PACOTE ITAU"),
                ("2026-10-06", 3990, "ESTORNO TARIFA"),
            ]
        );
        let t = &a.statement.transactions;
        assert_ne!(t[2].id, t[3].id, "two identical coffees stay two");
        assert!(a.checks[0].starts_with("Balance check: 3 of 3 balances add up."), "{:?}", a.checks);
        assert!(a.checks.iter().any(|c| c.starts_with("Whole period: the movements match")));
    }

    #[test]
    fn a_wrong_sign_shows_in_the_balance_check() {
        let flipped = CHECKING.replace("-150,00", " 150,00");
        let a = parse(&Doc::from_text(&flipped), &layout("itau_checking"));
        assert!(a.checks[0].starts_with("Balance check: 2 of 3"), "{:?}", a.checks);
        assert!(a.checks.iter().any(|c| c.contains("2026-10-02 off by R$ 300.00")), "{:?}", a.checks);
        assert!(a.checks.iter().any(|c| c.contains("off by R$ 300.00 from closing")), "{:?}", a.checks);
    }

    #[test]
    fn reads_a_card_bill() {
        let a = parse(&Doc::from_text(&card()), &layout("itau_card"));
        assert_eq!(a.statement.account, "4321");
        assert_eq!(a.statement.account_type, AccountType::CreditCard);
        assert_eq!(
            rows(&a.statement),
            vec![
                ("2026-10-12", -5890, "IFOOD *RESTAURANTE"), // after the "Lançamentos:" section label
                ("2026-10-10", 95_000, "PAGAMENTO EFETUADO"), // the payment is money in on the card
                ("2026-10-14", -15_000, "MAGAZINE LUIZA 02/10"),
                ("2026-10-18", -2250, "UBER *TRIP"), // "Total para próximas faturas" beside it doesn't stop reading
                ("2026-10-20", -12_000, "COCO BAMBU"),
                ("2026-10-22", -5590, "NETFLIX.COM"),
                ("2026-10-24", -21_000, "SUPERMERCADO BOMPRECO"),
                ("2026-10-25", -5540, "STEAMGAMES.COM WA USA"), // the R$ column, not US$
                ("2026-10-25", -194, "REPASSE DE IOF"),
            ]
        );
        assert_eq!(a.checks, vec!["Bill check: OK, the purchases add up to the bill's totals.".to_string()]);
    }

    #[test]
    fn reads_the_banks_category_under_each_purchase() {
        let a = parse(&Doc::from_text(&card()), &layout("itau_card"));
        let categories: Vec<(&str, Option<&str>)> =
            a.statement.transactions.iter().map(|t| (t.memo.as_str(), t.bank_category.as_deref())).collect();
        assert_eq!(
            categories,
            vec![
                ("IFOOD *RESTAURANTE", Some("restaurante")),
                ("PAGAMENTO EFETUADO", None),
                ("MAGAZINE LUIZA 02/10", Some("vestuário")),
                ("UBER *TRIP", Some("transporte")),
                ("COCO BAMBU", Some("restaurante")),
                ("NETFLIX.COM", Some("serviços")),
                ("SUPERMERCADO BOMPRECO", Some("supermercado")),
                ("STEAMGAMES.COM WA USA", None),
                ("REPASSE DE IOF", None),
            ]
        );
    }

    #[test]
    fn a_missing_purchase_shows_in_the_bill_check() {
        let text = card().replace("24/10   SUPERMERCADO BOMPRECO   210,00", "");
        let a = parse(&Doc::from_text(&text), &layout("itau_card"));
        assert_eq!(
            a.checks,
            vec!["Bill check: the purchases differ from the bill's totals by R$ 210.00.".to_string()]
        );
    }

    #[test]
    fn amounts_with_signs_and_marks() {
        assert_eq!(parse_amount("-1.234,56", None), Some(Money(-123_456)));
        assert_eq!(parse_amount("12.345,67", None), Some(Money(1_234_567)));
        assert_eq!(parse_amount("1.234,56-", None), Some(Money(-123_456)));
        assert_eq!(parse_amount("R$ 12,00", None), Some(Money(1200)));
        assert_eq!(parse_amount("- 9,99", None), Some(Money(-999)));
        assert_eq!(parse_amount("39,90", Some("D")), Some(Money(-3990)));
        assert_eq!(parse_amount("39,90", Some("c")), Some(Money(3990)));
        assert_eq!(full_date("05/10/26", None).as_deref(), Some("2026-10-05"));
        assert_eq!(full_date("05/10", None), None);
        assert_eq!(full_date("32/10/2026", None), None);
        assert_eq!(full_date("31/12", Some((2027, 1))).as_deref(), Some("2026-12-31"));
    }

    #[test]
    fn colors_from_the_xml() {
        assert_eq!(Tone::of_hex("#199933"), Tone::Green);
        assert_eq!(Tone::of_hex("#d81919"), Tone::Red);
        assert_eq!(Tone::of_hex("#000000"), Tone::Neutral);
        assert_eq!(Tone::of_hex("#2563eb"), Tone::Neutral);
        let xml = r##"<pdf2xml><page number="1">
            <fontspec id="0" size="12" family="Courier" color="#000000"/>
            <fontspec id="1" size="12" family="Courier" color="#d81919"/>
            <text top="55" left="30" width="72" height="9" font="0">02/10/2026</text>
            <text top="56" left="131" width="108" height="9" font="0"><b>CAF&#201; &amp; CIA</b></text>
            <text top="55" left="404" width="43" height="9" font="1">150,00</text>
            </page></pdf2xml>"##;
        let doc = Doc::from_xml(xml);
        let row = &doc.pages[0][0];
        assert!(row.text.starts_with("02/10/2026"), "{}", row.text);
        assert!(row.text.contains("CAFÉ & CIA"), "{}", row.text);
        let at = row.text.find("150,00").unwrap();
        assert_eq!(row.tone(at, at + 6), Tone::Red);
        assert_eq!(row.tone(0, 10), Tone::Neutral);
    }

    #[test]
    fn inspect_shows_no_letters_or_digits_from_the_file() {
        let secret = "SEGREDO FULANODETAL 98765-4 CPF 123.456.789-00";
        let text = format!("{CHECKING}{secret}\n");
        let report = inspect(&Doc::from_text(&text), &layouts());
        for word in ["SEGREDO", "FULANODETAL", "98765", "123.456", "SALARIO", "6.200", "12345-6", "ESQUINA"] {
            assert!(!report.contains(word), "{word} leaked:\n{report}");
        }
        for c in mask(&text).chars() {
            assert!(matches!(c, 'a' | 'A' | '9' | 'R') || !c.is_alphanumeric(), "{c:?} in a shape");
        }
        assert!(report.contains("Looks like: itau_checking"));
        assert!(report.contains("Transactions found: 6"));
        assert!(report.contains("99/99/9999   AAAAA AAAAAAA   9.999,99"), "{report}");
        assert!(report.contains("Balance check: 3 of 3 balances add up."), "{report}");
    }

    #[test]
    fn inspect_marks_what_the_importer_reads() {
        let report = inspect(&Doc::from_text(&card()), &layouts());
        let marked = |mark: &str| report.lines().filter(|l| l.starts_with(mark)).count();
        assert_eq!(marked("✓2"), 2, "two lines with two purchases each:\n{report}");
        assert_eq!(marked("■"), 1, "{report}");
        assert_eq!(marked("c "), 5, "category lines:\n{report}");
        assert!(report.lines().any(|l| l.starts_with("Σ")), "{report}");
        assert!(report.lines().any(|l| l.starts_with("i")), "{report}");
        // The due-date line holds a date and an amount but is skipped.
        let due = report.lines().find(|l| l.contains("Aaaaaaaaaa: 99/99/9999")).unwrap();
        assert!(due.starts_with("?"), "{report}");
        assert!(report.contains("Bill check: OK"));
    }

    /// One segment of a made-up PDF line: column (in characters), text, color.
    pub type Segment<'a> = (usize, &'a str, Tone);

    /// A one-page PDF in a fixed-width font. Each line is a list of segments placed at
    /// their column and printed in their color, like a real statement's pieces.
    pub fn make_pdf(lines: &[Vec<Segment>]) -> Vec<u8> {
        let mut content = String::from("BT /F1 8 Tf\n");
        for (i, line) in lines.iter().enumerate() {
            let y = 820 - i * 10;
            for (column, text, tone) in line {
                let color = match tone {
                    Tone::Neutral => "0 0 0 rg",
                    Tone::Green => "0.1 0.6 0.2 rg",
                    Tone::Red => "0.85 0.1 0.1 rg",
                };
                let mut escaped = String::new();
                for b in encoding_rs::WINDOWS_1252.encode(text).0.iter() {
                    match b {
                        b'(' | b')' | b'\\' => escaped += &format!("\\{}", *b as char),
                        0x20..=0x7e => escaped.push(*b as char),
                        _ => escaped += &format!("\\{b:03o}"),
                    }
                }
                content +=
                    &format!("{color} 1 0 0 1 {:.1} {y} Tm ({escaped}) Tj\n", 20.0 + *column as f64 * 4.8);
            }
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

    fn pdftohtml_available() -> bool {
        Command::new("pdftohtml").arg("-v").output().is_ok()
    }

    fn read_pdf(name: &str, lines: &[Vec<Segment>]) -> Doc {
        let path = std::env::temp_dir().join(format!("finance-test-{name}-{}.pdf", std::process::id()));
        std::fs::write(&path, make_pdf(lines)).unwrap();
        let doc = extract(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        doc
    }

    #[test]
    fn reads_a_real_pdf_card_bill() {
        if !pdftohtml_available() {
            eprintln!("pdftohtml not installed; skipping (it is in the Docker image)");
            return;
        }
        let card = card();
        let lines: Vec<Vec<Segment>> = card.lines().map(|l| vec![(0, l, Tone::Neutral)]).collect();
        let doc = read_pdf("card", &lines);
        let layouts = layouts();
        let layout = layouts.detect(&doc.text()).expect("detected");
        assert_eq!(layout.name, "itau_card");
        let a = parse(&doc, layout);
        assert_eq!(a.statement.transactions.len(), 9, "{}", doc.text());
        assert_eq!(a.checks, vec!["Bill check: OK, the purchases add up to the bill's totals.".to_string()]);
        assert_eq!(a.statement.transactions[2].bank_category.as_deref(), Some("vestuário"));
    }

    #[test]
    fn the_color_decides_the_sign_on_a_checking_statement() {
        if !pdftohtml_available() {
            eprintln!("pdftohtml not installed; skipping (it is in the Docker image)");
            return;
        }
        use Tone::{Green, Neutral, Red};
        let lines: Vec<Vec<Segment>> = vec![
            vec![(0, "ITAU UNIBANCO   Extrato conta corrente", Neutral)],
            vec![(0, "Agência 0001   Conta 12345-6", Neutral)],
            vec![(0, "30/09/2026", Neutral), (14, "SALDO ANTERIOR", Neutral), (72, "1.000,00", Neutral)],
            vec![(0, "01/10/2026", Neutral), (14, "PAGTO SALARIO", Neutral), (55, "6.200,00", Green)],
            vec![(0, "02/10/2026", Neutral), (14, "PIX TRANSF JOAO", Neutral), (57, "150,00", Red)], // red, no minus
            vec![
                (0, "02/10/2026", Neutral),
                (14, "CAFE DA ESQUINA", Neutral),
                (59, "5,00", Red),
                (72, "7.045,00", Neutral),
            ],
            vec![(0, "02/10/2026", Neutral), (14, "SALDO DO DIA", Neutral), (72, "7.045,00", Neutral)],
            vec![(0, "03/10/2026", Neutral), (14, "ESTORNO", Neutral), (58, "-10,00", Green)], // minus, but green
            vec![(0, "03/10/2026", Neutral), (14, "SALDO DO DIA", Neutral), (72, "7.055,00", Neutral)],
        ];
        let doc = read_pdf("checking", &lines);
        let layouts = layouts();
        let layout = layouts.detect(&doc.text()).expect("detected");
        assert_eq!(layout.name, "itau_checking");
        let a = parse(&doc, layout);
        assert_eq!(
            rows(&a.statement),
            vec![
                ("2026-10-01", 620_000, "PAGTO SALARIO"),
                ("2026-10-02", -15_000, "PIX TRANSF JOAO"),
                ("2026-10-02", -500, "CAFE DA ESQUINA"),
                ("2026-10-03", 1000, "ESTORNO"),
            ],
            "{}",
            doc.text()
        );
        assert!(a.checks[0].starts_with("Balance check: 2 of 2 balances add up."), "{:?}", a.checks);
        assert!(
            a.checks.iter().any(|c| c == "Color and minus sign disagree on line 1:8 (the color was used)."),
            "{:?}",
            a.checks
        );
        let report = inspect(&doc, &layouts);
        assert!(report.lines().any(|l| l.starts_with("✓− ")), "{report}");
        assert!(report.lines().any(|l| l.starts_with("✓+ ")), "{report}");
    }
}
