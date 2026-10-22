//! Importing OFX files into the event log.
//!
//! A file whose exact bytes were imported before is skipped. Otherwise every
//! transaction not seen before becomes a `TransactionObserved` event, followed by
//! one `StatementImported` event, all in one database transaction: a file is either
//! imported completely or not at all.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use duckdb::Connection;
use sha2::{Digest, Sha256};

use crate::events::Event;
use crate::ofx::{self, ParsedTransaction, Statement};
use crate::store;

/// Imports one .ofx file, or every .ofx file in a folder.
pub fn run(con: &Connection, target: &Path) -> Result<()> {
    let files = ofx_files(target)?;
    if files.is_empty() {
        println!("No .ofx files in {}.", target.display());
    }
    for file in files {
        import_file(con, &file).with_context(|| format!("importing {}", file.display()))?;
    }
    Ok(())
}

fn ofx_files(target: &Path) -> Result<Vec<PathBuf>> {
    if !target.is_dir() {
        return Ok(vec![target.to_path_buf()]);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(target).with_context(|| format!("reading {}", target.display()))? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ofx")) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn import_file(con: &Connection, path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if store::statement_exists(con, &sha256)? {
        println!("{name}: already imported (same file), skipped.");
        return Ok(());
    }

    let statement = ofx::parse(&bytes);
    let added = store::in_transaction(con, |con| record_statement(con, &statement, &name, &sha256))?;
    let total = statement.transactions.len();
    println!(
        "{name} ({} {}): {total} transactions in file, {added} new, {} already imported.",
        statement.account_type.as_str(),
        statement.account,
        total - added
    );
    Ok(())
}

/// Appends the events for one statement. Returns how many transactions were new.
fn record_statement(con: &Connection, statement: &Statement, file_name: &str, sha256: &str) -> Result<usize> {
    let mut added = 0;
    let mut seen = HashSet::new();
    for transaction in &statement.transactions {
        // Skip repeats within the file and transactions an earlier file already had.
        if !seen.insert(transaction.id.as_str()) || store::transaction_exists(con, &transaction.id)? {
            continue;
        }
        store::append(con, &observed(statement, transaction, file_name))?;
        added += 1;
    }
    store::append(
        con,
        &Event::StatementImported {
            file_name: file_name.to_string(),
            sha256: sha256.to_string(),
            account: statement.account.clone(),
            txn_count: statement.transactions.len() as i64,
        },
    )?;
    Ok(added)
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
