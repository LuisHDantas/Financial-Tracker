//! `finance serve`: the web page and the JSON API behind it.
//!
//! The page itself (static/index.html, app.css, app.js) is built into the binary
//! with `include_str!`, so the app is still one file and works offline. Everything
//! it shows comes from the JSON API under `/api`.
//!
//! The database is opened for each request and closed again, one request at a time
//! (DuckDB lets only one process write to the file). That keeps the CLI usable
//! while the server runs: `./finance summary` works between page loads.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ai::Ollama;
use crate::categorize::{self, Scope, Tally};
use crate::events::Event;
use crate::import::{self, FileResult};
use crate::money::Money;
use crate::pdf::Layouts;
use crate::rules::{Rules, TRANSFERS};
use crate::store::{self, CategorySpending, TransactionFilter, TransactionView};

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_CSS: &str = include_str!("../static/app.css");
const APP_JS: &str = include_str!("../static/app.js");

/// At most this many rows in the transactions list and the review queue.
const LIST_LIMIT: i64 = 500;
/// Months shown in the trend chart.
const TREND_MONTHS: i64 = 12;

/// Everything the handlers need, shared between requests.
pub struct Config {
    pub data_dir: PathBuf,
    pub rules_path: PathBuf,
    pub parsers_path: PathBuf,
    /// `None` when started with --no-ai.
    pub ollama: Option<(String, String)>, // (host, model)
}

#[derive(Clone)]
struct AppState(Arc<Shared>);

struct Shared {
    config: Config,
    /// One request at a time touches the database file.
    db: Mutex<()>,
}

/// Starts the server and runs until it's stopped.
pub fn serve(addr: SocketAddr, config: Config) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener =
            tokio::net::TcpListener::bind(addr).await.with_context(|| format!("listening on {addr}"))?;
        println!("Serving on http://{addr} (from your PC: http://localhost:8080)");
        axum::serve(listener, router(config)).await?;
        Ok(())
    })
}

fn router(config: Config) -> Router {
    let state = AppState(Arc::new(Shared { config, db: Mutex::new(()) }));
    Router::new()
        .route("/", get(|| async { page("text/html", INDEX_HTML) }))
        .route("/app.css", get(|| async { page("text/css", APP_CSS) }))
        .route("/app.js", get(|| async { page("text/javascript", APP_JS) }))
        .route("/api/categories", get(categories))
        .route("/api/months", get(months))
        .route("/api/summary", get(summary))
        .route("/api/trend", get(trend))
        .route("/api/transactions", get(transactions))
        .route("/api/review", get(review))
        .route("/api/transactions/:id/category", post(set_category))
        .route("/api/import", post(import_inbox))
        .with_state(state)
}

/// A built-in page file. `no-cache` makes the browser check for a newer version
/// each time, so an updated app shows up without a hard refresh.
fn page(mime: &'static str, body: &'static str) -> impl IntoResponse {
    let content_type = format!("{mime}; charset=utf-8");
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache".to_string())], body)
}

// --- Errors --------------------------------------------------------------------

/// An error sent back as `{"error": "..."}` with an HTTP status.
struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Runs `f` with the database (and rules.toml, read fresh so edits apply right
/// away) on a thread where blocking is fine, one request at a time.
async fn with_db<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&Connection, &Rules, &Config) -> Result<T, ApiError> + Send + 'static,
) -> ApiResult<T> {
    let shared = state.0.clone();
    tokio::task::spawn_blocking(move || {
        let _one_at_a_time = shared.db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let rules = Rules::load(&shared.config.rules_path)?;
        let con = store::open(&shared.config.data_dir)?;
        f(&con, &rules, &shared.config)
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!("request failed: {e}")))?
    .map(Json)
}

/// Query parameters arrive as "" when a filter is cleared; treat that as "any".
fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

// --- Handlers ------------------------------------------------------------------

#[derive(Serialize)]
struct CategoriesResponse {
    categories: Vec<String>,
}

async fn categories(State(state): State<AppState>) -> ApiResult<CategoriesResponse> {
    with_db(&state, |_, rules, _| Ok(CategoriesResponse { categories: rules.categories.clone() })).await
}

/// Months that have transactions, newest first.
async fn months(State(state): State<AppState>) -> ApiResult<Vec<String>> {
    with_db(&state, |con, _, _| {
        let mut months = store::recent_months(con, 1200)?;
        months.reverse();
        Ok(months)
    })
    .await
}

#[derive(Deserialize)]
struct MonthQuery {
    month: Option<String>,
}

#[derive(Serialize)]
struct SummaryResponse {
    month: Option<String>,
    money_in: Money,
    money_out: Money,
    net: Money,
    transfers_in: Money,
    transfers_out: Money,
    categories: Vec<CategorySpending>,
}

/// One month's totals and spending by category (the newest month by default).
async fn summary(State(state): State<AppState>, Query(q): Query<MonthQuery>) -> ApiResult<SummaryResponse> {
    with_db(&state, move |con, _, _| {
        let month = match non_empty(&q.month) {
            Some(month) => Some(month.to_string()),
            None => store::recent_months(con, 1)?.pop(),
        };
        let Some(month) = month else {
            return Ok(SummaryResponse {
                month: None,
                money_in: Money::ZERO,
                money_out: Money::ZERO,
                net: Money::ZERO,
                transfers_in: Money::ZERO,
                transfers_out: Money::ZERO,
                categories: Vec::new(),
            });
        };
        let t = store::month_totals(con, &month, TRANSFERS)?;
        Ok(SummaryResponse {
            categories: store::month_spending(con, &month, TRANSFERS)?,
            month: Some(month),
            net: t.money_in - t.money_out,
            money_in: t.money_in,
            money_out: t.money_out,
            transfers_in: t.transfers_in,
            transfers_out: t.transfers_out,
        })
    })
    .await
}

#[derive(Serialize)]
struct TrendPoint {
    month: String,
    money_in: Money,
    money_out: Money,
    by_category: BTreeMap<String, Money>,
}

/// Money in and out for each of the last 12 months that have transactions, oldest first.
async fn trend(State(state): State<AppState>) -> ApiResult<Vec<TrendPoint>> {
    with_db(&state, |con, _, _| {
        let mut points = Vec::new();
        for month in store::recent_months(con, TREND_MONTHS)? {
            let totals = store::month_totals(con, &month, TRANSFERS)?;
            let by_category = store::month_spending(con, &month, TRANSFERS)?
                .into_iter()
                .map(|s| (s.category, s.total))
                .collect();
            points.push(TrendPoint {
                month,
                money_in: totals.money_in,
                money_out: totals.money_out,
                by_category,
            });
        }
        Ok(points)
    })
    .await
}

#[derive(Deserialize)]
struct TransactionsQuery {
    month: Option<String>,
    category: Option<String>,
    account: Option<String>,
    q: Option<String>,
}

async fn transactions(
    State(state): State<AppState>,
    Query(q): Query<TransactionsQuery>,
) -> ApiResult<Vec<TransactionView>> {
    with_db(&state, move |con, _, _| {
        let filter = TransactionFilter {
            month: non_empty(&q.month),
            category: non_empty(&q.category),
            account_type: non_empty(&q.account),
            search: non_empty(&q.q),
        };
        Ok(store::list_transactions(con, &filter, LIST_LIMIT)?)
    })
    .await
}

/// What the AI categorized or nothing could, newest first.
async fn review(State(state): State<AppState>) -> ApiResult<Vec<TransactionView>> {
    with_db(&state, |con, _, _| Ok(store::review_queue(con, LIST_LIMIT)?)).await
}

#[derive(Deserialize)]
struct CategoryChange {
    category: String,
}

/// You choose a transaction's category: appends a `CategoryCorrected` event.
async fn set_category(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(change): Json<CategoryChange>,
) -> ApiResult<TransactionView> {
    with_db(&state, move |con, rules, _| {
        if !rules.categories.contains(&change.category) {
            return Err(ApiError::bad_request(format!("unknown category {:?}", change.category)));
        }
        if !store::transaction_exists(con, &id)? {
            return Err(ApiError::not_found(format!("no transaction {id:?}")));
        }
        let event = Event::CategoryCorrected { id: id.clone(), category: change.category };
        store::in_transaction(con, |con| store::append(con, &event))?;
        store::transaction(con, &id)?.ok_or_else(|| ApiError::not_found(format!("no transaction {id:?}")))
    })
    .await
}

#[derive(Serialize)]
struct ImportResponse {
    files: Vec<FileResult>,
    categorized: Tally,
}

/// Imports everything (.ofx, .pdf) in data/inbox and categorizes what's new, like
/// `finance import data/inbox`.
async fn import_inbox(State(state): State<AppState>) -> ApiResult<ImportResponse> {
    with_db(&state, |con, rules, config| {
        let inbox = config.data_dir.join("inbox");
        std::fs::create_dir_all(&inbox).with_context(|| format!("creating {}", inbox.display()))?;
        let layouts = Layouts::load(&config.parsers_path)?;
        let files = import::run(con, &inbox, &layouts)?;
        let ai = config.ollama.as_ref().map(|(host, model)| Ollama::new(host, model));
        let categorized = categorize::run(con, rules, ai, Scope::Uncategorized, true)?;
        Ok(ImportResponse { files, categorized })
    })
    .await
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    const RULES: &str = r#"
categories = ["Income", "Delivery", "Shopping", "Transfers", "Other"]

[[rules]]
pattern = "IFOOD"
category = "Delivery"

[[rules]]
pattern = "SALARIO"
category = "Income"
"#;

    const OFX: &str = "<ACCTID>1<BANKTRANLIST>\
        <STMTTRN><DTPOSTED>20261001<TRNAMT>3000.00<FITID>A<MEMO>PAGTO SALARIO\
        <STMTTRN><DTPOSTED>20261005<TRNAMT>-58.90<FITID>B<MEMO>IFOOD *REST\
        <STMTTRN><DTPOSTED>20261012<TRNAMT>-35.00<FITID>C<MEMO>LIVRARIA CULTURA</BANKTRANLIST>";

    /// A fresh data folder with rules.toml and one statement in the inbox; no AI.
    fn app(name: &str) -> (Router, PathBuf) {
        let dir = std::env::temp_dir().join(format!("finance-web-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("inbox")).unwrap();
        std::fs::write(dir.join("rules.toml"), RULES).unwrap();
        std::fs::write(dir.join("inbox/test.ofx"), OFX).unwrap();
        std::fs::write(dir.join("parsers.toml"), include_str!("../parsers.toml")).unwrap();
        let config = Config {
            data_dir: dir.clone(),
            rules_path: dir.join("rules.toml"),
            parsers_path: dir.join("parsers.toml"),
            ollama: None,
        };
        (router(config), dir)
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder().method(method).uri(uri).header("content-type", "application/json");
        let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
        let response = app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn import_then_browse_and_correct() {
        let (app, dir) = app("flow");

        let (status, imported) = call(&app, "POST", "/api/import", None).await;
        assert_eq!(status, StatusCode::OK, "{imported}");
        assert_eq!(imported["files"][0]["added"], 3);
        assert_eq!(imported["categorized"]["by_rule"], 2);

        let (_, months) = call(&app, "GET", "/api/months", None).await;
        assert_eq!(months, json!(["2026-10"]));

        let (_, summary) = call(&app, "GET", "/api/summary", None).await;
        assert_eq!(summary["month"], "2026-10");
        assert_eq!(summary["money_in"], 300_000);
        assert_eq!(summary["money_out"], 9390);

        // The bookstore matched no rule and there's no AI here, so it waits for review.
        let (_, queue) = call(&app, "GET", "/api/review", None).await;
        assert_eq!(queue.as_array().unwrap().len(), 1);
        let id = queue[0]["id"].as_str().unwrap().to_string();

        let (status, updated) = call(
            &app,
            "POST",
            &format!("/api/transactions/{id}/category"),
            Some(json!({"category": "Shopping"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        assert_eq!(updated["category"], "Shopping");
        assert_eq!(updated["category_source"], "manual");

        let (_, queue) = call(&app, "GET", "/api/review", None).await;
        assert_eq!(queue, json!([]));
        let (_, found) = call(&app, "GET", "/api/transactions?q=livraria&category=Shopping", None).await;
        assert_eq!(found.as_array().unwrap().len(), 1);

        // Importing again changes nothing and keeps your choice.
        let (_, again) = call(&app, "POST", "/api/import", None).await;
        assert_eq!(again["files"][0]["already_imported"], true);
        let (_, trend) = call(&app, "GET", "/api/trend", None).await;
        assert_eq!(trend[0]["by_category"]["Shopping"], 3500);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_unknown_categories_and_transactions() {
        let (app, dir) = app("errors");
        call(&app, "POST", "/api/import", None).await;
        let (status, body) =
            call(&app, "POST", "/api/transactions/nope/category", Some(json!({"category": "Shopping"})))
                .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let (_, list) = call(&app, "GET", "/api/transactions", None).await;
        let id = list[0]["id"].as_str().unwrap();
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/transactions/{id}/category"),
            Some(json!({"category": "Pizza"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("Pizza"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn serves_the_page() {
        let (app, dir) = app("page");
        let response = app.oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&html).contains("/app.js"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
