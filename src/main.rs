//! Personal finance tracker. Reads Itaú OFX statements (checking and credit card),
//! records everything as events in DuckDB, categorizes transactions (rules.toml
//! first, then a local model through Ollama) and prints a monthly summary.

mod ai;
mod events;
mod ofx;
mod rules;
mod store;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use duckdb::Connection;
use sha2::{Digest, Sha256};

use crate::ai::Ollama;
use crate::events::{CategorySource, Event};
use crate::rules::{Rules, FALLBACK, TRANSFERS};

#[derive(Parser)]
#[command(name = "finance", about = "Personal finance tracker: Itaú OFX statements, an event log in DuckDB, local AI categorization")]
struct Cli {
    /// Folder holding finance.duckdb
    #[arg(long, env = "FINANCE_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    /// Categories and rules file
    #[arg(long, env = "FINANCE_RULES", default_value = "rules.toml", global = true)]
    rules: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import an .ofx file or a folder of them
    Import {
        path: PathBuf,
        #[command(flatten)]
        ai: AiArgs,
    },
    /// Re-apply rules.toml (and the AI) to every transaction
    Recategorize {
        /// Ask the model again instead of reusing earlier AI answers
        #[arg(long)]
        clear_ai_cache: bool,
        #[command(flatten)]
        ai: AiArgs,
    },
    /// Income and spending by category per month
    Summary {
        #[arg(long, default_value_t = 3)]
        months: i64,
    },
    /// Print the most recent events from the log
    Events {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Rebuild the statements and transactions tables by replaying every event
    Rebuild,
}

#[derive(Args)]
struct AiArgs {
    /// Rules only, don't call Ollama
    #[arg(long)]
    no_ai: bool,
    /// Ollama model to use
    #[arg(long, env = "OLLAMA_MODEL", default_value = "llama3.2:3b")]
    model: String,
    #[arg(long, env = "OLLAMA_HOST", default_value = "http://localhost:11434", hide = true)]
    ollama_host: String,
}

impl AiArgs {
    fn client(&self) -> Option<Ollama> {
        (!self.no_ai).then(|| Ollama::new(&self.ollama_host, &self.model))
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let con = store::open(&cli.data_dir)?;
    match cli.command {
        Command::Import { path, ai } => {
            let rules = Rules::load(&cli.rules)?; // fail on a bad rules file before writing anything
            import(&con, &path)?;
            categorize(&con, &rules, ai.client(), true, false)
        }
        Command::Recategorize { clear_ai_cache, ai } => {
            let rules = Rules::load(&cli.rules)?;
            categorize(&con, &rules, ai.client(), false, clear_ai_cache)
        }
        Command::Summary { months } => summary(&con, months),
        Command::Events { limit } => {
            for e in store::recent_events(&con, limit)? {
                println!("#{:<5} {}  {:<22} {}", e.seq, e.occurred_at, e.kind, e.payload);
            }
            Ok(())
        }
        Command::Rebuild => {
            let n = store::rebuild(&con)?;
            println!("Replayed {n} events into the statements and transactions tables.");
            Ok(())
        }
    }
}

fn import(con: &Connection, target: &Path) -> Result<()> {
    let files = ofx_files(target)?;
    if files.is_empty() {
        println!("No .ofx files in {}.", target.display());
    }
    for file in files {
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if store::statement_exists(con, &sha256)? {
            println!("{name}: already imported (same file), skipped.");
            continue;
        }
        let statement = ofx::parse(&bytes);
        let added = store::in_transaction(con, |con| {
            let mut added = 0;
            let mut seen = HashSet::new();
            for t in &statement.transactions {
                if !seen.insert(t.id.as_str()) || store::transaction_exists(con, &t.id)? {
                    continue;
                }
                store::append(
                    con,
                    &Event::TransactionObserved {
                        id: t.id.clone(),
                        account: statement.account.clone(),
                        account_type: statement.account_type,
                        fitid: t.fitid.clone(),
                        date: t.date.clone(),
                        amount_cents: t.amount_cents,
                        memo: t.memo.clone(),
                        trn_type: t.trn_type.clone(),
                        source_file: name.clone(),
                    },
                )?;
                added += 1;
            }
            store::append(
                con,
                &Event::StatementImported {
                    file_name: name.clone(),
                    sha256: sha256.clone(),
                    account: statement.account.clone(),
                    txn_count: statement.transactions.len() as i64,
                },
            )?;
            Ok(added)
        })?;
        let n = statement.transactions.len();
        println!(
            "{name} ({} {}): {n} transactions in file, {added} new, {} already imported.",
            statement.account_type.as_str(),
            statement.account,
            n - added
        );
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

/// Rules first, then the AI. Appends a TransactionCategorized event only when a
/// transaction's category or source actually changes.
fn categorize(
    con: &Connection,
    rules: &Rules,
    mut ai: Option<Ollama>,
    only_uncategorized: bool,
    clear_ai_cache: bool,
) -> Result<()> {
    let rows = store::transactions_to_categorize(con, only_uncategorized)?;
    let mut answered: HashMap<String, String> = HashMap::new(); // memo_key -> category, this run
    let (mut by_rule, mut by_ai, mut left, mut changed) = (0, 0, 0, 0);

    for row in &rows {
        let mut decision = rules.category_for(&row.memo).map(|c| (c.to_string(), CategorySource::Rule, None));

        if decision.is_none() {
            let mut ai_failed = false;
            if let Some(client) = &ai {
                let earlier = match answered.get(&row.memo_key) {
                    Some(category) => Some(category.clone()),
                    None if !clear_ai_cache => store::ai_category_for(con, &row.memo_key)?,
                    None => None,
                };
                let answer = match earlier {
                    Some(category) => Some(category),
                    None => match client.categorize(&row.memo, row.amount_cents, &rules.categories) {
                        Ok(answer) => answer,
                        Err(e) => {
                            println!("Ollama failed ({e:#}); skipping AI for the rest of this run. Is the ollama container running?");
                            ai_failed = true;
                            None
                        }
                    },
                };
                if let Some(category) = answer {
                    answered.insert(row.memo_key.clone(), category.clone());
                    decision = Some((category, CategorySource::Ai, Some(client.model.clone())));
                }
            }
            if ai_failed {
                ai = None;
            }
        }

        // No rule and no new AI answer: keep an earlier AI answer rather than dropping to the fallback.
        if decision.is_none() && row.category_source.as_deref() == Some(CategorySource::Ai.as_str()) {
            if let Some(category) = &row.category {
                decision = Some((category.clone(), CategorySource::Ai, row.model.clone()));
            }
        }

        let (category, source, model) =
            decision.unwrap_or_else(|| (FALLBACK.to_string(), CategorySource::None, None));
        match source {
            CategorySource::Rule => by_rule += 1,
            CategorySource::Ai => by_ai += 1,
            CategorySource::None => left += 1,
        }
        if row.category.as_deref() != Some(category.as_str()) || row.category_source.as_deref() != Some(source.as_str())
        {
            store::in_transaction(con, |con| {
                store::append(con, &Event::TransactionCategorized { id: row.id.clone(), category, source, model })
            })?;
            changed += 1;
        }
    }
    println!(
        "Categorized {} transactions: {by_rule} by rules, {by_ai} by AI, {left} left as '{FALLBACK}' ({changed} changed).",
        rows.len()
    );
    Ok(())
}

fn summary(con: &Connection, months: i64) -> Result<()> {
    let months = store::recent_months(con, months)?;
    if months.is_empty() {
        println!("No transactions yet. Import a statement first: finance import data/inbox");
    }
    for month in &months {
        let t = store::month_totals(con, month, TRANSFERS)?;
        println!(
            "\n=== {month} ===   in: R$ {}   out: R$ {}   net: R$ {}",
            money(t.money_in),
            money(t.money_out),
            money(t.money_in - t.money_out)
        );
        for (category, total, n) in store::month_spending(con, month, TRANSFERS)? {
            let share = if t.money_out > 0 { total as f64 / t.money_out as f64 * 100.0 } else { 0.0 };
            println!("  {category:<16} R$ {:>10}  {share:5.1}%  ({n} txns)", money(total));
        }
        if t.transfers_in != 0 || t.transfers_out != 0 {
            println!(
                "  transfers (not counted above): R$ {} in, R$ {} out",
                money(t.transfers_in),
                money(t.transfers_out)
            );
        }
    }
    let unsure = store::uncategorized(con, 10)?;
    if !unsure.is_empty() {
        println!("\nStill uncategorized (add a rule to rules.toml, then run `recategorize`):");
        for (date, cents, memo) in unsure {
            println!("  {date}  R$ {:>10}  {memo}", money(cents));
        }
    }
    Ok(())
}

/// -123456 -> "-1,234.56"
pub fn money(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    let int = (abs / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}{grouped}.{:02}", if cents < 0 { "-" } else { "" }, abs % 100)
}

#[cfg(test)]
mod tests {
    use super::money;

    #[test]
    fn formats_money() {
        assert_eq!(money(0), "0.00");
        assert_eq!(money(5), "0.05");
        assert_eq!(money(-5890), "-58.90");
        assert_eq!(money(123_456_789), "1,234,567.89");
    }
}
