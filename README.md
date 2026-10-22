# Financial Tracker

Personal finance tracker for my own use, written in Rust. It reads Itaú OFX
statements (checking account and credit card), skips transactions it already has,
categorizes them for free (rules first, then a local model through Ollama), keeps
everything in DuckDB and prints a monthly summary. Everything runs in Docker, so
nothing is installed on the host and no bank data leaves the machine.

## Start

From WSL Ubuntu, in the repo folder:

```bash
./start.sh
```

then open **http://localhost:8080**. It starts the local AI, downloads the model the
first time (~1.9 GB, kept in a Docker volume), builds the app and starts the web
page. Run it again after a restart; it's quick once everything is downloaded.
`docker compose down` stops everything. The page is only reachable from this PC.

## The web page

- **Overview**: a month's money in, out, net and transfers; spending by category
  (click a bar to see its transactions); money in and out over the last 12 months.
- **Transactions**: filter by month, category and account, search descriptions.
  Click a category to change it.
- **Review**: what the AI categorized, or nothing could. Confirm the guess or pick
  another category. Your choice is final: rules and the AI never overwrite it, it
  survives `rebuild`, and it's reused for similar descriptions.
- **Import**: imports everything in `data/inbox/`.

## Monthly use

1. Download the checking statement and the credit card bill from Itaú, as PDF or
   OFX (internet banking → Conta corrente → Extrato → "Salvar em outros formatos" →
   **OFX / Money 2000**; OFX is the more reliable of the two when it's offered).
2. Save the files into `data/inbox/` (from Windows: `\\wsl$\Ubuntu\home\<user>\projects\Financial-Tracker\data\inbox`).
3. Press **Import** on the page, then go through **Review**.

Don't import the same period as both PDF and OFX: PDFs carry no transaction ids,
so the two can't be matched and you'd get each transaction twice.

## Importing Itaú PDFs

PDF statements are read with `pdftotext` (inside Docker) and the patterns in
`parsers.toml`: one layout for the checking statement (`itau_checking`), one for the
card bill (`itau_card`). The app picks the layout whose keywords appear most. Card
purchases count as money out and the bill payment as a transfer; dates without a
year get it from the bill's due date; balance lines, totals and "próximas faturas"
(future installments) are skipped. Two identical rows (two R$ 5,00 coffees on one
day) stay two transactions, and importing the same PDF again adds nothing.

These layouts were written from what Itaú statements usually look like and tested
with made-up PDFs, so a real one may need a small change in `parsers.toml`.

### If a PDF doesn't import

Run:

```bash
./finance inspect data/inbox/your-file.pdf
```

It prints only the *shape* of each line: every letter becomes `a`/`A` and every
digit `9`, so `05/10 IFOOD *RESTAURANTE 58,90` shows as
`99/99 AAAAA *AAAAAAAAAAA 99,99`. No names, places or amounts. It also marks which
lines would be read as transactions (✓) and where reading stops (■). Check the
output yourself first, and share a few lines only if you want to; they're enough to
adjust `parsers.toml`.

The same from the command line:

```bash
./finance import data/inbox
./finance summary
```

`./finance` runs the app in Docker (`docker compose run --rm app ...`); `./finance --help`
lists every command. It works while the web page is running.

Importing the same file twice is safe: a file already imported is skipped, and so
is any transaction already in the database.

## Categories

`rules.toml` holds the category list, the only categories anything may use, the
rules (regexes on the description) that run before the AI, and `[[examples]]` that
are shown to the AI with every question. A rule or example pointing to a category
that isn't in the list is an error. When the AI gets something wrong, add a rule
(or, for a kind of merchant no single pattern covers, an example) and run:

```bash
./finance recategorize
```

The AI is a small local model, `qwen2.5:3b`, so expect it to get roughly half of
the unusual descriptions right; rules do most of the work.

`Transfers` is money moved between your own accounts, including paying the card
bill. The summary shows it on its own line and leaves it out of income and spending,
so card purchases aren't counted twice.

Other options: `--no-ai` (rules only), `--model llama3.2:3b` (another Ollama model, pulled first),
`recategorize --clear-ai-cache` (ask the model again), `summary --months 6`.

## How the data is stored

The source of truth is an append-only event log, the `events` table:

- `StatementImported`: a file was read (its sha256 stops it being imported twice)
- `TransactionObserved`: a transaction was seen for the first time
- `TransactionCategorized`: a transaction got a new category, from a rule, the AI or neither
- `CategoryCorrected`: you chose a transaction's category on the web page

The `statements` and `transactions` tables are projections of the log, updated in
the same database transaction as each event. They can be thrown away and rebuilt:

```bash
./finance events --limit 20   # look at the log
./finance rebuild             # replay it into fresh tables
```

## Development

The `dev` service has the Rust toolchain and the prebuilt DuckDB library, with the
source mounted from the host and the build cache in Docker volumes, so rebuilds are
incremental (debug profile):

```bash
docker compose build dev
docker compose run --rm dev cargo test
docker compose run --rm dev cargo run -- import data/inbox
docker compose run --rm dev cargo run -- summary
```

DuckDB isn't compiled from source: the Dockerfile downloads the official
`libduckdb-linux-amd64.zip`. Its `DUCKDB_VERSION` must match the `duckdb` crate in
`Cargo.toml` (crate `1.10506.x` = DuckDB 1.5.6); update both together.

## Data

`data/` holds the statements and `finance.duckdb`. It is in `.gitignore` and must
never be committed. Back it up encrypted if you back it up at all.
