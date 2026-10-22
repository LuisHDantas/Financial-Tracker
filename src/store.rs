//! DuckDB storage: the append-only `events` table plus the `statements` and
//! `transactions` projections, which are updated in the same database transaction
//! as each append.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use duckdb::{params, Connection};

use crate::events::Event;

const EVENTS_SCHEMA: &str = "
CREATE SEQUENCE IF NOT EXISTS event_seq START 1;
CREATE TABLE IF NOT EXISTS events (
    seq         BIGINT PRIMARY KEY DEFAULT nextval('event_seq'),
    kind        VARCHAR NOT NULL,
    occurred_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    payload     VARCHAR NOT NULL            -- the event as JSON
);
";

const PROJECTIONS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS statements (
    sha256    VARCHAR PRIMARY KEY,          -- hash of the file's bytes
    file_name VARCHAR,
    account   VARCHAR,
    txn_count INTEGER,
    seq       BIGINT
);
CREATE TABLE IF NOT EXISTS transactions (
    id              VARCHAR PRIMARY KEY,    -- hash of account|fitid|date|cents|memo
    account         VARCHAR,
    account_type    VARCHAR,                -- 'checking' or 'credit_card'
    fitid           VARCHAR,
    date            DATE,
    amount_cents    BIGINT,                 -- negative = money out
    memo            VARCHAR,
    memo_key        VARCHAR,                -- memo without digits, so similar memos share an AI answer
    trn_type        VARCHAR,
    source_file     VARCHAR,
    category        VARCHAR,
    category_source VARCHAR,                -- 'rule', 'ai' or 'none'
    model           VARCHAR,
    last_seq        BIGINT                  -- last event applied to this row
);
";

pub fn open(data_dir: &Path) -> Result<Connection> {
    fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let con = Connection::open(data_dir.join("finance.duckdb"))?;
    con.execute_batch(EVENTS_SCHEMA)?;
    con.execute_batch(PROJECTIONS_SCHEMA)?;
    Ok(con)
}

pub fn in_transaction<T>(con: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    con.execute_batch("BEGIN TRANSACTION")?;
    match f(con) {
        Ok(value) => {
            con.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(e) => {
            let _ = con.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Appends an event and applies it to the projections. Call inside `in_transaction`.
pub fn append(con: &Connection, event: &Event) -> Result<i64> {
    let payload = serde_json::to_string(event)?;
    let seq: i64 = con.query_row(
        "INSERT INTO events (kind, payload) VALUES (?, ?) RETURNING seq",
        params![event.kind(), payload],
        |row| row.get(0),
    )?;
    apply(con, seq, event)?;
    Ok(seq)
}

fn apply(con: &Connection, seq: i64, event: &Event) -> Result<()> {
    match event {
        Event::StatementImported { file_name, sha256, account, txn_count } => {
            con.execute(
                "INSERT OR IGNORE INTO statements (sha256, file_name, account, txn_count, seq) VALUES (?, ?, ?, ?, ?)",
                params![sha256.as_str(), file_name.as_str(), account.as_str(), *txn_count, seq],
            )?;
        }
        Event::TransactionObserved {
            id,
            account,
            account_type,
            fitid,
            date,
            amount_cents,
            memo,
            trn_type,
            source_file,
        } => {
            con.execute(
                "INSERT OR IGNORE INTO transactions \
                 (id, account, account_type, fitid, date, amount_cents, memo, memo_key, trn_type, source_file, last_seq) \
                 VALUES (?, ?, ?, ?, CAST(? AS DATE), ?, ?, ?, ?, ?, ?)",
                params![
                    id.as_str(),
                    account.as_str(),
                    account_type.as_str(),
                    fitid.as_str(),
                    date.as_str(),
                    *amount_cents,
                    memo.as_str(),
                    memo_key(memo),
                    trn_type.as_deref(),
                    source_file.as_str(),
                    seq
                ],
            )?;
        }
        Event::TransactionCategorized { id, category, source, model } => {
            con.execute(
                "UPDATE transactions SET category = ?, category_source = ?, model = ?, last_seq = ? WHERE id = ?",
                params![category.as_str(), source.as_str(), model.as_deref(), seq, id.as_str()],
            )?;
        }
    }
    Ok(())
}

/// Drops the projections and replays every event. Returns the number of events.
pub fn rebuild(con: &Connection) -> Result<usize> {
    in_transaction(con, |con| {
        con.execute_batch("DROP TABLE statements; DROP TABLE transactions;")?;
        con.execute_batch(PROJECTIONS_SCHEMA)?;
        let mut stmt = con.prepare("SELECT seq, payload FROM events ORDER BY seq")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .collect::<duckdb::Result<Vec<_>>>()?;
        for (seq, payload) in &rows {
            let event: Event = serde_json::from_str(payload).with_context(|| format!("event #{seq} is not valid"))?;
            apply(con, *seq, &event)?;
        }
        Ok(rows.len())
    })
}

/// "PIX TRANSF JOAO S15/10" -> "PIX TRANSF JOAO S"
pub fn memo_key(memo: &str) -> String {
    memo.to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_digit() || "/.-:".contains(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// --- Queries on the projections ----------------------------------------------

pub fn statement_exists(con: &Connection, sha256: &str) -> Result<bool> {
    let n: i64 = con.query_row("SELECT count(*) FROM statements WHERE sha256 = ?", params![sha256], |r| r.get(0))?;
    Ok(n > 0)
}

pub fn transaction_exists(con: &Connection, id: &str) -> Result<bool> {
    let n: i64 = con.query_row("SELECT count(*) FROM transactions WHERE id = ?", params![id], |r| r.get(0))?;
    Ok(n > 0)
}

pub struct TransactionRow {
    pub id: String,
    pub memo: String,
    pub memo_key: String,
    pub amount_cents: i64,
    pub category: Option<String>,
    pub category_source: Option<String>,
    pub model: Option<String>,
}

pub fn transactions_to_categorize(con: &Connection, only_uncategorized: bool) -> Result<Vec<TransactionRow>> {
    let filter = if only_uncategorized { "WHERE category IS NULL OR category_source = 'none'" } else { "" };
    let mut stmt = con.prepare(&format!(
        "SELECT id, memo, memo_key, amount_cents, category, category_source, model \
         FROM transactions {filter} ORDER BY date, id"
    ))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(TransactionRow {
                id: r.get(0)?,
                memo: r.get(1)?,
                memo_key: r.get(2)?,
                amount_cents: r.get(3)?,
                category: r.get(4)?,
                category_source: r.get(5)?,
                model: r.get(6)?,
            })
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

/// An earlier AI answer for a similar memo, reused instead of asking the model again.
pub fn ai_category_for(con: &Connection, memo_key: &str) -> Result<Option<String>> {
    let mut stmt =
        con.prepare("SELECT category FROM transactions WHERE memo_key = ? AND category_source = 'ai' LIMIT 1")?;
    Ok(stmt.query_map(params![memo_key], |r| r.get(0))?.next().transpose()?)
}

/// The last `limit` months that have transactions, oldest first.
pub fn recent_months(con: &Connection, limit: i64) -> Result<Vec<String>> {
    let mut stmt =
        con.prepare("SELECT DISTINCT strftime(date, '%Y-%m') AS m FROM transactions ORDER BY m DESC LIMIT ?")?;
    let mut months = stmt.query_map(params![limit], |r| r.get(0))?.collect::<duckdb::Result<Vec<String>>>()?;
    months.reverse();
    Ok(months)
}

pub struct MonthTotals {
    pub money_in: i64,
    pub money_out: i64,
    pub transfers_in: i64,
    pub transfers_out: i64,
}

/// Totals in cents for one month. Transactions in the `transfers` category (moving
/// money between your own accounts, paying the card bill) are kept apart so they
/// don't count as income or spending.
pub fn month_totals(con: &Connection, month: &str, transfers: &str) -> Result<MonthTotals> {
    Ok(con.query_row(
        "SELECT CAST(coalesce(sum(amount_cents) FILTER (WHERE amount_cents > 0 AND NOT t), 0) AS BIGINT), \
                CAST(coalesce(-sum(amount_cents) FILTER (WHERE amount_cents < 0 AND NOT t), 0) AS BIGINT), \
                CAST(coalesce(sum(amount_cents) FILTER (WHERE amount_cents > 0 AND t), 0) AS BIGINT), \
                CAST(coalesce(-sum(amount_cents) FILTER (WHERE amount_cents < 0 AND t), 0) AS BIGINT) \
         FROM (SELECT amount_cents, coalesce(category = ?, false) AS t FROM transactions \
               WHERE strftime(date, '%Y-%m') = ?)",
        params![transfers, month],
        |r| {
            Ok(MonthTotals {
                money_in: r.get(0)?,
                money_out: r.get(1)?,
                transfers_in: r.get(2)?,
                transfers_out: r.get(3)?,
            })
        },
    )?)
}

/// (category, cents spent, number of transactions) for one month, biggest first,
/// leaving out the `transfers` category.
pub fn month_spending(con: &Connection, month: &str, transfers: &str) -> Result<Vec<(String, i64, i64)>> {
    let mut stmt = con.prepare(
        "SELECT coalesce(category, 'Other') AS c, CAST(-sum(amount_cents) AS BIGINT) AS total, count(*) \
         FROM transactions WHERE amount_cents < 0 AND strftime(date, '%Y-%m') = ? \
         GROUP BY 1 HAVING c <> ? ORDER BY total DESC, 1",
    )?;
    let rows = stmt
        .query_map(params![month, transfers], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

/// (date, cents, memo) of transactions nothing could categorize, newest first.
pub fn uncategorized(con: &Connection, limit: i64) -> Result<Vec<(String, i64, String)>> {
    let mut stmt = con.prepare(
        "SELECT CAST(date AS VARCHAR), amount_cents, memo FROM transactions \
         WHERE category_source = 'none' ORDER BY date DESC LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct EventRow {
    pub seq: i64,
    pub occurred_at: String,
    pub kind: String,
    pub payload: String,
}

/// The last `limit` events, oldest first.
pub fn recent_events(con: &Connection, limit: i64) -> Result<Vec<EventRow>> {
    let mut stmt = con.prepare(
        "SELECT seq, strftime(occurred_at, '%Y-%m-%d %H:%M:%S'), kind, payload \
         FROM events ORDER BY seq DESC LIMIT ?",
    )?;
    let mut rows = stmt
        .query_map(params![limit], |r| {
            Ok(EventRow { seq: r.get(0)?, occurred_at: r.get(1)?, kind: r.get(2)?, payload: r.get(3)? })
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    rows.reverse();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memo_key_drops_digits_and_separators() {
        assert_eq!(memo_key("PIX TRANSF JOAO S15/10"), "PIX TRANSF JOAO S");
        assert_eq!(memo_key("ifood *rest 12:30"), "IFOOD *REST");
    }
}
