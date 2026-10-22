//! DuckDB storage: the event log and the projections built from it.
//!
//! Writing always goes through [`append`], which inserts the event into the
//! append-only `events` table and then [`apply`]s it to the projections
//! (`statements`, `transactions`) in the same database transaction, so the two can
//! never disagree. [`rebuild`] drops the projections and applies every event again
//! in order, which must give the same tables: `apply` only looks at the event, never
//! at the clock or the network.
//!
//! Everything else here is a read-only query on the projections.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use duckdb::{params, Connection, Row};

use crate::events::{CategorySource, Event};
use crate::money::Money;

// --- Schema ------------------------------------------------------------------

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

const DROP_PROJECTIONS: &str = "DROP TABLE statements; DROP TABLE transactions;";

// --- Writes ------------------------------------------------------------------

const INSERT_EVENT: &str = "INSERT INTO events (kind, payload) VALUES (?, ?) RETURNING seq";

const INSERT_STATEMENT: &str =
    "INSERT OR IGNORE INTO statements (sha256, file_name, account, txn_count, seq) VALUES (?, ?, ?, ?, ?)";

const INSERT_TRANSACTION: &str = "
INSERT OR IGNORE INTO transactions
    (id, account, account_type, fitid, date, amount_cents, memo, memo_key, trn_type, source_file, last_seq)
VALUES (?, ?, ?, ?, CAST(? AS DATE), ?, ?, ?, ?, ?, ?)";

const SET_CATEGORY: &str =
    "UPDATE transactions SET category = ?, category_source = ?, model = ?, last_seq = ? WHERE id = ?";

const ALL_EVENTS: &str = "SELECT seq, payload FROM events ORDER BY seq";

// --- Reads -------------------------------------------------------------------

const STATEMENT_EXISTS: &str = "SELECT count(*) FROM statements WHERE sha256 = ?";

const TRANSACTION_EXISTS: &str = "SELECT count(*) FROM transactions WHERE id = ?";

const TRANSACTIONS_TO_CATEGORIZE: &str = "
SELECT id, memo, memo_key, amount_cents, category, category_source, model
FROM transactions
WHERE NOT CAST(? AS BOOLEAN) OR category IS NULL OR category_source = 'none'
ORDER BY date, id";

const EARLIER_AI_ANSWER: &str =
    "SELECT category FROM transactions WHERE memo_key = ? AND category_source = 'ai' LIMIT 1";

const RECENT_MONTHS: &str =
    "SELECT DISTINCT strftime(date, '%Y-%m') AS m FROM transactions ORDER BY m DESC LIMIT ?";

/// `t` marks transfers, which are summed separately from income and spending.
const MONTH_TOTALS: &str = "
SELECT CAST(coalesce(sum(amount_cents) FILTER (WHERE amount_cents > 0 AND NOT t), 0) AS BIGINT),
       CAST(coalesce(-sum(amount_cents) FILTER (WHERE amount_cents < 0 AND NOT t), 0) AS BIGINT),
       CAST(coalesce(sum(amount_cents) FILTER (WHERE amount_cents > 0 AND t), 0) AS BIGINT),
       CAST(coalesce(-sum(amount_cents) FILTER (WHERE amount_cents < 0 AND t), 0) AS BIGINT)
FROM (SELECT amount_cents, coalesce(category = ?, false) AS t
      FROM transactions WHERE strftime(date, '%Y-%m') = ?)";

const MONTH_SPENDING: &str = "
SELECT coalesce(category, 'Other') AS c, CAST(-sum(amount_cents) AS BIGINT) AS total, count(*)
FROM transactions
WHERE amount_cents < 0 AND strftime(date, '%Y-%m') = ?
GROUP BY 1 HAVING c <> ?
ORDER BY total DESC, 1";

const UNCATEGORIZED: &str = "
SELECT CAST(date AS VARCHAR), amount_cents, memo FROM transactions
WHERE category_source = 'none' ORDER BY date DESC LIMIT ?";

const RECENT_EVENTS: &str = "
SELECT seq, strftime(occurred_at, '%Y-%m-%d %H:%M:%S'), kind, payload
FROM events ORDER BY seq DESC LIMIT ?";

// --- Connection and transactions ---------------------------------------------

/// Opens (or creates) `data_dir/finance.duckdb` and makes sure the tables exist.
pub fn open(data_dir: &Path) -> Result<Connection> {
    fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join("finance.duckdb");
    let con = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
    create_tables(&con)?;
    Ok(con)
}

#[cfg(test)]
pub fn open_in_memory() -> Result<Connection> {
    let con = Connection::open_in_memory()?;
    create_tables(&con)?;
    Ok(con)
}

fn create_tables(con: &Connection) -> Result<()> {
    con.execute_batch(EVENTS_SCHEMA).context("creating the events table")?;
    con.execute_batch(PROJECTIONS_SCHEMA).context("creating the projection tables")?;
    Ok(())
}

/// Runs `f` in a database transaction: everything it writes is saved together, or
/// nothing is if it returns an error.
pub fn in_transaction<T>(con: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    con.execute_batch("BEGIN TRANSACTION")?;
    match f(con) {
        Ok(value) => {
            con.execute_batch("COMMIT").context("saving changes")?;
            Ok(value)
        }
        Err(e) => {
            let _ = con.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// --- Event log ---------------------------------------------------------------

/// Appends an event to the log and applies it to the projections. Returns its
/// sequence number. Call it inside [`in_transaction`].
pub fn append(con: &Connection, event: &Event) -> Result<i64> {
    let payload = serde_json::to_string(event)?;
    let seq: i64 = con
        .query_row(INSERT_EVENT, params![event.kind(), payload], |row| row.get(0))
        .with_context(|| format!("appending a {} event", event.kind()))?;
    apply(con, seq, event)?;
    Ok(seq)
}

/// Updates the projections for one event.
fn apply(con: &Connection, seq: i64, event: &Event) -> Result<()> {
    match event {
        Event::StatementImported { file_name, sha256, account, txn_count } => {
            con.execute(INSERT_STATEMENT, params![sha256, file_name, account, txn_count, seq])?;
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
                INSERT_TRANSACTION,
                params![
                    id,
                    account,
                    account_type.as_str(),
                    fitid,
                    date,
                    amount_cents,
                    memo,
                    memo_key(memo),
                    trn_type,
                    source_file,
                    seq
                ],
            )?;
        }
        Event::TransactionCategorized { id, category, source, model } => {
            con.execute(SET_CATEGORY, params![category, source.as_str(), model, seq, id])?;
        }
    }
    Ok(())
}

/// Drops the projections and replays every event into fresh ones. Returns the
/// number of events replayed.
pub fn rebuild(con: &Connection) -> Result<usize> {
    in_transaction(con, |con| {
        con.execute_batch(DROP_PROJECTIONS)?;
        con.execute_batch(PROJECTIONS_SCHEMA)?;
        let mut stmt = con.prepare(ALL_EVENTS)?;
        let events = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .collect::<duckdb::Result<Vec<_>>>()?;
        for (seq, payload) in &events {
            let event: Event =
                serde_json::from_str(payload).with_context(|| format!("event #{seq} can't be read"))?;
            apply(con, *seq, &event).with_context(|| format!("replaying event #{seq}"))?;
        }
        Ok(events.len())
    })
}

/// The memo in capitals without digits and separators, so the same merchant on
/// different days shares one AI answer: "PIX TRANSF JOAO S15/10" -> "PIX TRANSF JOAO S".
pub fn memo_key(memo: &str) -> String {
    memo.to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_digit() || "/.-:".contains(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// --- Queries -------------------------------------------------------------------

pub fn statement_exists(con: &Connection, sha256: &str) -> Result<bool> {
    let n: i64 = con.query_row(STATEMENT_EXISTS, params![sha256], |r| r.get(0))?;
    Ok(n > 0)
}

pub fn transaction_exists(con: &Connection, id: &str) -> Result<bool> {
    let n: i64 = con.query_row(TRANSACTION_EXISTS, params![id], |r| r.get(0))?;
    Ok(n > 0)
}

/// A transaction as the categorizer sees it.
pub struct TransactionRow {
    pub id: String,
    pub memo: String,
    pub memo_key: String,
    pub amount: Money,
    pub category: Option<String>,
    pub category_source: Option<CategorySource>,
    pub model: Option<String>,
}

impl TransactionRow {
    fn from_row(r: &Row<'_>) -> duckdb::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            memo: r.get(1)?,
            memo_key: r.get(2)?,
            amount: r.get(3)?,
            category: r.get(4)?,
            category_source: r.get::<_, Option<String>>(5)?.as_deref().and_then(CategorySource::parse),
            model: r.get(6)?,
        })
    }
}

/// Every transaction, or only the ones without a real category yet, oldest first.
pub fn transactions_to_categorize(con: &Connection, only_uncategorized: bool) -> Result<Vec<TransactionRow>> {
    let mut stmt = con.prepare(TRANSACTIONS_TO_CATEGORIZE)?;
    let rows = stmt
        .query_map(params![only_uncategorized], TransactionRow::from_row)?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

/// An earlier AI answer for a similar memo, reused instead of asking the model again.
pub fn earlier_ai_answer(con: &Connection, memo_key: &str) -> Result<Option<String>> {
    let mut stmt = con.prepare(EARLIER_AI_ANSWER)?;
    Ok(stmt.query_map(params![memo_key], |r| r.get(0))?.next().transpose()?)
}

/// The last `limit` months ("2026-10") that have transactions, oldest first.
pub fn recent_months(con: &Connection, limit: i64) -> Result<Vec<String>> {
    let mut stmt = con.prepare(RECENT_MONTHS)?;
    let mut months =
        stmt.query_map(params![limit], |r| r.get(0))?.collect::<duckdb::Result<Vec<String>>>()?;
    months.reverse();
    Ok(months)
}

/// One month's money in and out. Transfers (moving money between your own
/// accounts, paying the card bill) are kept apart so they count as neither.
pub struct MonthTotals {
    pub money_in: Money,
    pub money_out: Money,
    pub transfers_in: Money,
    pub transfers_out: Money,
}

pub fn month_totals(con: &Connection, month: &str, transfers_category: &str) -> Result<MonthTotals> {
    Ok(con.query_row(MONTH_TOTALS, params![transfers_category, month], |r| {
        Ok(MonthTotals {
            money_in: r.get(0)?,
            money_out: r.get(1)?,
            transfers_in: r.get(2)?,
            transfers_out: r.get(3)?,
        })
    })?)
}

pub struct CategorySpending {
    pub category: String,
    pub total: Money,
    pub count: i64,
}

/// What one month's spending went to, biggest first, without transfers.
pub fn month_spending(
    con: &Connection,
    month: &str,
    transfers_category: &str,
) -> Result<Vec<CategorySpending>> {
    let mut stmt = con.prepare(MONTH_SPENDING)?;
    let rows = stmt
        .query_map(params![month, transfers_category], |r| {
            Ok(CategorySpending { category: r.get(0)?, total: r.get(1)?, count: r.get(2)? })
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

pub struct UncategorizedRow {
    pub date: String,
    pub amount: Money,
    pub memo: String,
}

/// Transactions nothing could categorize, newest first.
pub fn uncategorized(con: &Connection, limit: i64) -> Result<Vec<UncategorizedRow>> {
    let mut stmt = con.prepare(UNCATEGORIZED)?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(UncategorizedRow { date: r.get(0)?, amount: r.get(1)?, memo: r.get(2)? })
        })?
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
    let mut stmt = con.prepare(RECENT_EVENTS)?;
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
    use crate::events::AccountType;

    fn observed(id: &str, memo: &str, cents: i64) -> Event {
        Event::TransactionObserved {
            id: id.into(),
            account: "1".into(),
            account_type: AccountType::Checking,
            fitid: id.into(),
            date: "2026-10-05".into(),
            amount_cents: Money(cents),
            memo: memo.into(),
            trn_type: None,
            source_file: "test.ofx".into(),
        }
    }

    fn categorized(id: &str, category: &str) -> Event {
        Event::TransactionCategorized {
            id: id.into(),
            category: category.into(),
            source: CategorySource::Rule,
            model: None,
        }
    }

    #[test]
    fn memo_key_drops_digits_and_separators() {
        assert_eq!(memo_key("PIX TRANSF JOAO S15/10"), "PIX TRANSF JOAO S");
        assert_eq!(memo_key("ifood *rest 12:30"), "IFOOD *REST");
    }

    #[test]
    fn append_updates_the_projections() {
        let con = open_in_memory().unwrap();
        in_transaction(&con, |con| {
            append(con, &observed("a", "IFOOD", -5890))?;
            append(con, &categorized("a", "Delivery"))
        })
        .unwrap();
        let rows = transactions_to_categorize(&con, false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].amount, Money(-5890));
        assert_eq!(rows[0].category.as_deref(), Some("Delivery"));
        assert_eq!(rows[0].category_source, Some(CategorySource::Rule));
        assert!(transaction_exists(&con, "a").unwrap());
    }

    #[test]
    fn rebuild_gives_the_same_projections() {
        let con = open_in_memory().unwrap();
        in_transaction(&con, |con| {
            append(con, &observed("a", "IFOOD", -5890))?;
            append(con, &observed("b", "ITAU CARTAO PAG FAT", -95_000))?;
            append(con, &categorized("a", "Delivery"))?;
            append(con, &categorized("b", "Transfers"))?;
            append(con, &categorized("a", "Restaurants")) // a later event wins
        })
        .unwrap();
        let before = month_spending(&con, "2026-10", "Transfers").unwrap();
        assert_eq!(rebuild(&con).unwrap(), 5);
        let after = month_spending(&con, "2026-10", "Transfers").unwrap();
        let summarize = |rows: &[CategorySpending]| -> Vec<(String, i64)> {
            rows.iter().map(|r| (r.category.clone(), r.total.0)).collect()
        };
        assert_eq!(summarize(&before), vec![("Restaurants".to_string(), 5890)]);
        assert_eq!(summarize(&before), summarize(&after));
    }

    #[test]
    fn a_failed_transaction_writes_nothing() {
        let con = open_in_memory().unwrap();
        let result: Result<()> = in_transaction(&con, |con| {
            append(con, &observed("a", "IFOOD", -5890))?;
            anyhow::bail!("something went wrong")
        });
        assert!(result.is_err());
        assert!(!transaction_exists(&con, "a").unwrap());
        assert!(recent_events(&con, 10).unwrap().is_empty());
    }

    #[test]
    fn transfers_are_kept_out_of_income_and_spending() {
        let con = open_in_memory().unwrap();
        in_transaction(&con, |con| {
            append(con, &observed("s", "PAGTO SALARIO", 300_000))?;
            append(con, &observed("f", "ITAU CARTAO PAG FAT", -95_000))?;
            append(con, &observed("i", "IFOOD", -5890))?;
            append(con, &categorized("s", "Income"))?;
            append(con, &categorized("f", "Transfers"))?;
            append(con, &categorized("i", "Delivery"))
        })
        .unwrap();
        let t = month_totals(&con, "2026-10", "Transfers").unwrap();
        assert_eq!((t.money_in, t.money_out), (Money(300_000), Money(5890)));
        assert_eq!((t.transfers_in, t.transfers_out), (Money::ZERO, Money(95_000)));
    }
}
