//! The monthly summary: income, spending by category, and transfers on their own
//! line, so paying the card bill isn't counted on top of the card purchases.

use anyhow::Result;
use duckdb::Connection;

use crate::money::Money;
use crate::rules::TRANSFERS;
use crate::store;

/// How many still-uncategorized transactions to list at the end.
const UNCATEGORIZED_SHOWN: i64 = 10;

/// Prints the last `months` months that have transactions, oldest first.
pub fn print(con: &Connection, months: i64) -> Result<()> {
    let months = store::recent_months(con, months)?;
    if months.is_empty() {
        println!("No transactions yet. Import a statement first: finance import data/inbox");
    }
    for month in &months {
        print_month(con, month)?;
    }
    print_uncategorized(con)
}

fn print_month(con: &Connection, month: &str) -> Result<()> {
    let totals = store::month_totals(con, month, TRANSFERS)?;
    println!(
        "\n=== {month} ===   in: R$ {}   out: R$ {}   net: R$ {}",
        totals.money_in,
        totals.money_out,
        totals.money_in - totals.money_out
    );
    for spending in store::month_spending(con, month, TRANSFERS)? {
        println!(
            "  {:<16} R$ {:>10}  {:5.1}%  ({} txns)",
            spending.category,
            spending.total,
            spending.total.percent_of(totals.money_out),
            spending.count
        );
    }
    if totals.transfers_in != Money::ZERO || totals.transfers_out != Money::ZERO {
        println!(
            "  transfers (not counted above): R$ {} in, R$ {} out",
            totals.transfers_in, totals.transfers_out
        );
    }
    Ok(())
}

fn print_uncategorized(con: &Connection) -> Result<()> {
    let rows = store::uncategorized(con, UNCATEGORIZED_SHOWN)?;
    if rows.is_empty() {
        return Ok(());
    }
    println!("\nStill uncategorized (add a rule to rules.toml, then run `recategorize`):");
    for row in rows {
        println!("  {}  R$ {:>10}  {}", row.date, row.amount, row.memo);
    }
    Ok(())
}
