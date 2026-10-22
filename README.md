# Financial Tracker

Personal finance tracker for my own use, written in Rust. It reads Itaú OFX
statements (checking account and credit card), skips transactions it already has,
categorizes them for free (rules first, then a local model through Ollama), keeps
everything in DuckDB and prints a monthly summary. Everything runs in Docker, so
nothing is installed on the host and no bank data leaves the machine.

## Setup (once)

From WSL Ubuntu, in the repo folder:

```bash
docker compose build app                              # release image
docker compose up -d ollama
docker compose exec ollama ollama pull llama3.2:3b   # ~2 GB, stored in a Docker volume
mkdir -p data/inbox
```

## Monthly use

1. Itaú internet banking → Conta corrente → Extrato por período → "Salvar em outros
   formatos" → **OFX / Money 2000**. Do the same for the credit card statement if
   it offers OFX, so card purchases are counted one by one.
2. Save the files into `data/inbox/` (from Windows: `\\wsl$\Ubuntu-22.04\home\<user>\projects\Financial-Tracker\data\inbox`).
3. Run:

```bash
docker compose up -d ollama
docker compose run --rm app import data/inbox
docker compose run --rm app summary
```

Importing the same file twice is safe: a file already imported is skipped, and so
is any transaction already in the database.

## Categories

`rules.toml` holds the category list, the only categories anything may use, and the
rules (regexes on the description) that run before the AI. A rule pointing to a
category that isn't in the list is an error. When the AI gets something wrong, add
a rule and run:

```bash
docker compose run --rm app recategorize
```

`Transfers` is money moved between your own accounts, including paying the card
bill. The summary shows it on its own line and leaves it out of income and spending,
so card purchases aren't counted twice.

Other options: `--no-ai` (rules only), `--model qwen2.5:3b` (another Ollama model),
`recategorize --clear-ai-cache` (ask the model again), `summary --months 6`.

## How the data is stored

The source of truth is an append-only event log, the `events` table:

- `StatementImported`: a file was read (its sha256 stops it being imported twice)
- `TransactionObserved`: a transaction was seen for the first time
- `TransactionCategorized`: a transaction got a new category, from a rule, the AI or neither

The `statements` and `transactions` tables are projections of the log, updated in
the same database transaction as each event. They can be thrown away and rebuilt:

```bash
docker compose run --rm app events --limit 20   # look at the log
docker compose run --rm app rebuild             # replay it into fresh tables
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
