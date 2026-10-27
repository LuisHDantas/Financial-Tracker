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
use serde::Serialize;

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
    category_source VARCHAR,                -- 'manual', 'rule', 'ai' or 'none'
    model           VARCHAR,
    last_seq        BIGINT,                 -- last event applied to this row
    statement_sha256 VARCHAR                -- the statement file that added it
);
";

/// Brings a projection created by an older version up to date.
const MIGRATIONS: &str = "ALTER TABLE transactions ADD COLUMN IF NOT EXISTS statement_sha256 VARCHAR;";

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

const SET_MANUAL_CATEGORY: &str =
    "UPDATE transactions SET category = ?, category_source = 'manual', model = NULL, last_seq = ? WHERE id = ?";

const LINK_TO_STATEMENT: &str = "UPDATE transactions SET statement_sha256 = ? WHERE id = ?";

/// Statements imported before transactions were linked to them are matched by file name.
const DELETE_STATEMENT_TRANSACTIONS: &str =
    "DELETE FROM transactions WHERE statement_sha256 = ? OR (statement_sha256 IS NULL AND source_file = ?)";

const DELETE_STATEMENT: &str = "DELETE FROM statements WHERE sha256 = ?";

const ALL_EVENTS: &str = "SELECT seq, payload FROM events ORDER BY seq";

/// Imported statements matching a file name or the start of a sha256.
const FIND_STATEMENTS: &str = "
SELECT s.sha256, s.file_name,
       (SELECT count(*) FROM transactions t
        WHERE t.statement_sha256 = s.sha256 OR (t.statement_sha256 IS NULL AND t.source_file = s.file_name))
FROM statements s
WHERE s.file_name = ? OR s.sha256 LIKE ? || '%'
ORDER BY s.seq";

// --- Reads -------------------------------------------------------------------

const STATEMENT_EXISTS: &str = "SELECT count(*) FROM statements WHERE sha256 = ?";

const TRANSACTION_EXISTS: &str = "SELECT count(*) FROM transactions WHERE id = ?";

const TRANSACTIONS_TO_CATEGORIZE: &str = "
SELECT id, memo, memo_key, amount_cents, category, category_source, model
FROM transactions
WHERE NOT CAST(? AS BOOLEAN) OR category IS NULL OR category_source = 'none'
ORDER BY date, id";

/// Your own choice for a similar memo wins over an earlier AI answer.
const EARLIER_AI_ANSWER: &str = "
SELECT category FROM transactions
WHERE memo_key = ? AND category_source IN ('manual', 'ai')
ORDER BY category_source = 'manual' DESC, last_seq DESC
LIMIT 1";

const TRANSACTION_COLUMNS: &str =
    "id, CAST(date AS VARCHAR), amount_cents, memo, account_type, category, category_source, model";

/// Every filter is optional: NULL means "any".
const LIST_TRANSACTIONS: &str = "
WHERE (CAST($1 AS VARCHAR) IS NULL OR strftime(date, '%Y-%m') = $1)
  AND (CAST($2 AS VARCHAR) IS NULL OR category = $2)
  AND (CAST($3 AS VARCHAR) IS NULL OR account_type = $3)
  AND (CAST($4 AS VARCHAR) IS NULL OR memo ILIKE '%' || $4 || '%')
ORDER BY date DESC, id
LIMIT $5";

/// What the AI chose or nothing could categorize: the transactions worth a look.
const REVIEW_QUEUE: &str = "
WHERE category_source IN ('ai', 'none') OR category_source IS NULL
ORDER BY date DESC, id
LIMIT ?";

const ONE_TRANSACTION: &str = "WHERE id = ?";

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
    con.execute_batch(MIGRATIONS).context("updating the projection tables")?;
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
        Event::StatementImported { file_name, sha256, account, txn_count, transaction_ids } => {
            con.execute(INSERT_STATEMENT, params![sha256, file_name, account, txn_count, seq])?;
            for id in transaction_ids {
                con.execute(LINK_TO_STATEMENT, params![sha256, id])?;
            }
        }
        Event::StatementRetracted { file_name, sha256 } => {
            con.execute(DELETE_STATEMENT_TRANSACTIONS, params![sha256, file_name])?;
            con.execute(DELETE_STATEMENT, params![sha256])?;
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
        Event::CategoryCorrected { id, category } => {
            con.execute(SET_MANUAL_CATEGORY, params![category, seq, id])?;
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

/// An imported statement: (sha256, file name, how many transactions it added).
pub type StatementSummary = (String, String, i64);

/// Imported statements whose file name is `key` or whose sha256 starts with it.
pub fn find_statements(con: &Connection, key: &str) -> Result<Vec<StatementSummary>> {
    let mut stmt = con.prepare(FIND_STATEMENTS)?;
    let rows = stmt
        .query_map(params![key, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
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

/// An earlier answer for a similar memo (yours, or else the AI's), reused instead of
/// asking the model again.
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
#[derive(Serialize)]
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

#[derive(Serialize)]
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

/// A transaction as the web pages show it.
#[derive(Debug, Serialize)]
pub struct TransactionView {
    pub id: String,
    pub date: String,
    pub amount: Money,
    pub memo: String,
    pub account_type: Option<String>,
    pub category: Option<String>,
    pub category_source: Option<String>,
    pub model: Option<String>,
}

impl TransactionView {
    fn from_row(r: &Row<'_>) -> duckdb::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            date: r.get(1)?,
            amount: r.get(2)?,
            memo: r.get(3)?,
            account_type: r.get(4)?,
            category: r.get(5)?,
            category_source: r.get(6)?,
            model: r.get(7)?,
        })
    }
}

/// Filters for [`list_transactions`]; `None` means "any".
#[derive(Default)]
pub struct TransactionFilter<'a> {
    pub month: Option<&'a str>,
    pub category: Option<&'a str>,
    pub account_type: Option<&'a str>,
    pub search: Option<&'a str>,
}

/// Transactions matching `filter`, newest first, at most `limit`.
pub fn list_transactions(
    con: &Connection,
    filter: &TransactionFilter<'_>,
    limit: i64,
) -> Result<Vec<TransactionView>> {
    let mut stmt =
        con.prepare(&format!("SELECT {TRANSACTION_COLUMNS} FROM transactions {LIST_TRANSACTIONS}"))?;
    let rows = stmt
        .query_map(
            params![filter.month, filter.category, filter.account_type, filter.search, limit],
            TransactionView::from_row,
        )?
        .collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Transactions the AI categorized or nothing could, newest first.
pub fn review_queue(con: &Connection, limit: i64) -> Result<Vec<TransactionView>> {
    let mut stmt = con.prepare(&format!("SELECT {TRANSACTION_COLUMNS} FROM transactions {REVIEW_QUEUE}"))?;
    let rows =
        stmt.query_map(params![limit], TransactionView::from_row)?.collect::<duckdb::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn transaction(con: &Connection, id: &str) -> Result<Option<TransactionView>> {
    let mut stmt =
        con.prepare(&format!("SELECT {TRANSACTION_COLUMNS} FROM transactions {ONE_TRANSACTION}"))?;
    Ok(stmt.query_map(params![id], TransactionView::from_row)?.next().transpose()?)
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

    fn imported(sha: &str, ids: &[&str]) -> Event {
        Event::StatementImported {
            file_name: "test.ofx".into(),
            sha256: sha.into(),
            account: "1".into(),
            txn_count: ids.len() as i64,
            transaction_ids: ids.iter().map(|id| id.to_string()).collect(),
        }
    }

    #[test]
    fn a_retracted_statement_can_be_imported_again_and_rebuild_agrees() {
        let con = open_in_memory().unwrap();
        in_transaction(&con, |con| {
            append(con, &observed("a", "IFOOD", -5890))?;
            append(con, &observed("b", "PADARIA", -1000))?;
            append(con, &imported("sha1", &["a", "b"]))
        })
        .unwrap();
        assert_eq!(find_statements(&con, "test.ofx").unwrap(), vec![("sha1".into(), "test.ofx".into(), 2)]);
        assert_eq!(find_statements(&con, "sha").unwrap().len(), 1);

        in_transaction(&con, |con| {
            append(con, &Event::StatementRetracted { file_name: "test.ofx".into(), sha256: "sha1".into() })
        })
        .unwrap();
        assert!(!transaction_exists(&con, "a").unwrap());
        assert!(!statement_exists(&con, "sha1").unwrap());

        // The same file again, read better this time (one transaction fixed).
        in_transaction(&con, |con| {
            append(con, &observed("a", "IFOOD", -5890))?;
            append(con, &observed("c", "PADARIA", 1000))?;
            append(con, &imported("sha1", &["a", "c"]))
        })
        .unwrap();
        let ids = |con: &Connection| -> Vec<String> {
            list_transactions(con, &TransactionFilter::default(), 10)
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect()
        };
        let before = ids(&con);
        assert_eq!(before.len(), 2);
        assert_eq!(rebuild(&con).unwrap(), 7);
        assert_eq!(ids(&con), before);
        assert!(statement_exists(&con, "sha1").unwrap());
    }

    #[test]
    fn a_manual_correction_survives_a_rebuild() {
        let con = open_in_memory().unwrap();
        in_transaction(&con, |con| {
            append(con, &observed("a", "LIVRARIA CULTURA", -3500))?;
            append(con, &categorized("a", "Delivery"))?;
            append(con, &Event::CategoryCorrected { id: "a".into(), category: "Shopping".into() })
        })
        .unwrap();
        for _ in 0..2 {
            let t = transaction(&con, "a").unwrap().unwrap();
            assert_eq!(t.category.as_deref(), Some("Shopping"));
            assert_eq!(t.category_source.as_deref(), Some("manual"));
            assert_eq!(t.model, None);
            rebuild(&con).unwrap();
        }
        assert!(review_queue(&con, 10).unwrap().is_empty());
        // Your answer is reused for similar memos, ahead of any AI answer.
        assert_eq!(earlier_ai_answer(&con, "LIVRARIA CULTURA").unwrap().as_deref(), Some("Shopping"));
    }

    #[test]
    fn lists_transactions_with_optional_filters() {
        let con = open_in_memory().unwrap();
        in_transaction(&con, |con| {
            append(con, &observed("a", "IFOOD *REST", -5890))?;
            append(con, &observed("b", "LIVRARIA CULTURA", -3500))?;
            append(con, &categorized("a", "Delivery"))
        })
        .unwrap();
        let all = list_transactions(&con, &TransactionFilter::default(), 100).unwrap();
        assert_eq!(all.len(), 2);
        let search = TransactionFilter { search: Some("livraria"), ..Default::default() };
        let found = list_transactions(&con, &search, 100).unwrap();
        assert_eq!(found.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["b"]);
        let by_category =
            TransactionFilter { category: Some("Delivery"), month: Some("2026-10"), ..Default::default() };
        assert_eq!(list_transactions(&con, &by_category, 100).unwrap().len(), 1);
        let other_month = TransactionFilter { month: Some("2026-09"), ..Default::default() };
        assert!(list_transactions(&con, &other_month, 100).unwrap().is_empty());
        // "b" has no category yet, so it waits for review.
        assert_eq!(review_queue(&con, 10).unwrap().len(), 1);
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
