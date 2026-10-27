# Financial Tracker: notes for Claude Code

Personal finance tracker for Itaú statements (OFX or PDF), with local AI
categorization. Read README.md for how it's used.

## Standing rules

- **Rust only.** The app is a Rust binary (`finance`, crate `financial-tracker`).
  Never add Python or another backend language.
- **Frontend stays plain**: `static/index.html`, `app.css`, `app.js`, vanilla JS
  with charts drawn as inline SVG, no build step, no npm, nothing from a CDN.
  The files are compiled into the binary and served by axum (`src/web.rs`).
  If a charting library is ever needed, vendor it into `static/` (e.g. Chart.js).
- **Event-sourced DuckDB.** The `events` table is the source of truth and is
  append-only. `statements` and `transactions` are projections updated in the same
  database transaction as each event (`store::append` → `apply`). `finance rebuild`
  replays every event and must give the same tables, so `apply` may only look at the
  event. New behavior = a new event type + its `apply` arm; old events must keep
  deserializing (new fields get `#[serde(default)]`). Schema changes for existing
  databases go in `store::MIGRATIONS`.
- **Everything runs in Docker** (WSL Ubuntu): `./start.sh` starts Ollama + the web
  page (http://localhost:8080); `./finance <command>` runs the CLI in the release
  image; the `dev` service is for cargo:
  `docker compose run --rm dev cargo test` / `cargo clippy --all-targets -- -D warnings`
  / `cargo fmt`. Keep tests, clippy and fmt clean; never skip or disable a test.
- **PDF import**: `src/pdf.rs` reads `pdftohtml -xml` output (text + position +
  color), rebuilds lines keeping the columns, and applies the layouts in
  `parsers.toml` (`itau_checking`, `itau_card`). Prefer adjusting `parsers.toml` over
  code. Checking amounts: green = money in, red = money out, color wins over a minus
  sign. Each import prints checks: the checking balance check (SALDO lines must add
  up) and the card bill check (purchases must add up to the bill's totals).
- Categories: `rules.toml` (category list, one-line meanings, regex rules, examples
  for the AI). The AI is `qwen2.5:3b` through Ollama; `finance bench` scores it on
  `bench/descriptions.toml` (tune on `dev`, check `test` once per change).

## PRIVACY (strict)

- **Never open, read, cat, grep, pdftotext, pdftohtml, `finance inspect`, import or
  query anything in `data/`** (statements, `finance.duckdb`) or any of the user's
  real statements, wherever they are. Never ask the user to share them.
- Moving or listing her files by name is fine; reading their content is not.
  Before any test that clears `data/`, move her files aside and put them back.
- Test only with synthetic data: the fixtures in `src/pdf.rs` (`CHECKING`, `card()`,
  `make_pdf`) and made-up OFX. When a real statement doesn't parse, ask her to run
  `./finance inspect <file>` herself; it prints only line shapes (letters a/A,
  digits 9) and she decides whether to share a few lines. Reproduce what she shares
  in synthetic fixtures.

## Open work

- Itaú card category line (lowercase under each purchase): parsed into
  `ParsedTransaction.bank_category` but not stored yet. Next: add it to
  `TransactionObserved` (optional field), a `[itau_categories]` mapping in
  rules.toml applied after the rules and before the AI, and pass it to the AI as a hint.
- Card bill fixes (done in code, checked only with synthetic bills): two-column
  "Total para próximas faturas" no longer stops reading; the "Vencimento: ... = Total
  desta fatura" line is skipped; international purchases use the R$ column; the
  "Repasse de IOF" line becomes a fee. Needs her to re-run `./finance inspect` on a
  real bill: the count should go up and "Bill check: OK". The purchase line she saw
  missed (page 2 line 72, shape `99/99   AAAAAAAAAAAA AAAAAAAA   999,99`) reads fine
  in the synthetic bill; if it's still missed, `inspect` now shows why (`?s`/`?l`/`?d`).
- Summary/Overview: split money in/out into Income, Spending, Transfers and
  Investments (investment moves like APLIC/RESGATE AUT are still counted in in/out).
- Transfer rule variants for the card payment in checking (PAG FATURA, PGTO CARTAO,
  CARTAO CREDITO, ITAU UNICLASS/PERSONNALITE...).
- Web: a "Remove" button per imported statement on the Import page (the CLI has
  `./finance forget <file>`), and showing the import checks there.
- A manual category (CategoryCorrected) on a statement that is forgotten and
  imported again is lost; reapplying it by transaction id would be nice.
