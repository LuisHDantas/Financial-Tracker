//! Importing statement files (OFX, or PDF read with the layouts in parsers.toml)
//! into the event log.
//!
//! A file whose exact bytes were imported before is skipped. Otherwise every
//! transaction not seen before becomes a `TransactionObserved` event, followed by
//! one `StatementImported` event, all in one database transaction: a file is either
//! imported completely or not at all.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use duckdb::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::events::Event;
use crate::ofx::{self, ParsedTransaction, Statement};
use crate::pdf::{self, Layouts};
use crate::store;

/// What happened to one file.
#[derive(Debug, Serialize)]
pub struct FileResult {
    pub file: String,
    /// The exact same file was imported before, so nothing was read.
    pub already_imported: bool,
    pub account: Option<String>,
    pub account_type: Option<&'static str>,
    pub in_file: usize,
    pub added: usize,
    /// PDF checks (balances add up, bill totals match, color conflicts).
    pub checks: Vec<String>,
}

impl fmt::Display for FileResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.already_imported {
            return write!(f, "{}: already imported (same file), skipped.", self.file);
        }
        write!(
            f,
            "{} ({} {}): {} transactions in file, {} new, {} already imported.",
            self.file,
            self.account_type.unwrap_or_default(),
            self.account.as_deref().unwrap_or_default(),
            self.in_file,
            self.added,
            self.in_file - self.added
        )?;
        for check in &self.checks {
            write!(f, "\n  {check}")?;
        }
        Ok(())
    }
}

/// Imports one .ofx/.pdf file, or every such file in a folder (none is not an error).
pub fn run(con: &Connection, target: &Path, layouts: &Layouts) -> Result<Vec<FileResult>> {
    statement_files(target)?
        .iter()
        .map(|file| import_file(con, file, layouts).with_context(|| format!("importing {}", file.display())))
        .collect()
}

fn is_pdf(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

fn statement_files(target: &Path) -> Result<Vec<PathBuf>> {
    if !target.is_dir() {
        return Ok(vec![target.to_path_buf()]);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(target).with_context(|| format!("reading {}", target.display()))? {
        let path = entry?.path();
        let known = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ofx")) || is_pdf(&path);
        if path.is_file() && known {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Reads a statement: OFX directly, PDF through the best-fitting layout (which
/// also gives the checks).
fn read_statement(path: &Path, bytes: &[u8], layouts: &Layouts) -> Result<(Statement, Vec<String>)> {
    if !is_pdf(path) {
        return Ok((ofx::parse(bytes), Vec::new()));
    }
    let doc = pdf::extract(path)?;
    let Some(layout) = layouts.detect(&doc.text()) else {
        bail!("this PDF doesn't look like any layout in parsers.toml; run `./finance inspect` on it");
    };
    let analysis = pdf::parse(&doc, layout);
    if analysis.statement.transactions.is_empty() {
        bail!(
            "no transactions found with the {} layout; run `./finance inspect` on it and adjust parsers.toml",
            layout.name
        );
    }
    Ok((analysis.statement, analysis.checks))
}

fn import_file(con: &Connection, path: &Path, layouts: &Layouts) -> Result<FileResult> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if store::statement_exists(con, &sha256)? {
        return Ok(FileResult {
            file: name,
            already_imported: true,
            account: None,
            account_type: None,
            in_file: 0,
            added: 0,
            checks: Vec::new(),
        });
    }

    let (statement, checks) = read_statement(path, &bytes, layouts)?;
    let added = store::in_transaction(con, |con| record_statement(con, &statement, &name, &sha256))?;
    Ok(FileResult {
        file: name,
        already_imported: false,
        account_type: Some(statement.account_type.as_str()),
        in_file: statement.transactions.len(),
        account: Some(statement.account),
        added,
        checks,
    })
}

/// Appends the events for one statement. Returns how many transactions were new.
fn record_statement(con: &Connection, statement: &Statement, file_name: &str, sha256: &str) -> Result<usize> {
    let mut added = Vec::new();
    let mut seen = HashSet::new();
    for transaction in &statement.transactions {
        // Skip repeats within the file and transactions an earlier file already had.
        if !seen.insert(transaction.id.as_str()) || store::transaction_exists(con, &transaction.id)? {
            continue;
        }
        store::append(con, &observed(statement, transaction, file_name))?;
        added.push(transaction.id.clone());
    }
    let count = added.len();
    store::append(
        con,
        &Event::StatementImported {
            file_name: file_name.to_string(),
            sha256: sha256.to_string(),
            account: statement.account.clone(),
            txn_count: statement.transactions.len() as i64,
            transaction_ids: added,
        },
    )?;
    Ok(count)
}

fn observed(statement: &Statement, transaction: &ParsedTransaction, file_name: &str) -> Event {
    Event::TransactionObserved {
        id: transaction.id.clone(),
        account: statement.account.clone(),
        account_type: statement.account_type,
        fitid: transaction.fitid.clone(),
        date: transaction.date.clone(),
        amount_cents: transaction.amount,
        memo: transaction.memo.clone(),
        trn_type: transaction.trn_type.clone(),
        source_file: file_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFX: &str = "<ACCTID>1<BANKTRANLIST>\
        <STMTTRN><DTPOSTED>20261005<TRNAMT>-58.90<FITID>A<MEMO>IFOOD\
        <STMTTRN><DTPOSTED>20261005<TRNAMT>-58.90<FITID>A<MEMO>IFOOD\
        <STMTTRN><DTPOSTED>20261006<TRNAMT>-10.00<FITID>B<MEMO>PADARIA</BANKTRANLIST>";

    #[test]
    fn records_each_transaction_once() {
        let con = store::open_in_memory().unwrap();
        let statement = ofx::parse(OFX.as_bytes());
        let added =
            store::in_transaction(&con, |con| record_statement(con, &statement, "a.ofx", "h1")).unwrap();
        assert_eq!(added, 2); // the duplicate line inside the file counts once
        let again =
            store::in_transaction(&con, |con| record_statement(con, &statement, "b.ofx", "h2")).unwrap();
        assert_eq!(again, 0); // a second file with the same transactions adds nothing
        assert!(store::statement_exists(&con, "h1").unwrap());
    }
}
