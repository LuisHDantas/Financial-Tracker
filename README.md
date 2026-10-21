# Financial Tracker

Personal finance tracker for my own use. It reads Itaú OFX statements, skips
transactions it already has, categorizes them for free (rules first, then a
local model through Ollama), stores everything in DuckDB and prints a monthly
summary. Everything runs in Docker, so nothing is installed on the host and no
bank data leaves the machine.

## Setup (once)

From WSL Ubuntu, in the repo folder:

```bash
docker compose build
docker compose up -d ollama
docker compose exec ollama ollama pull llama3.2:3b   # ~2 GB, stored in a Docker volume
mkdir -p data/inbox
```

## Monthly use

1. Itaú internet banking → Conta corrente → Extrato por período → "Salvar em outros
   formatos" → **OFX / Money 2000**.
2. Save the file into `data/inbox/` (from Windows: `\\wsl$\Ubuntu-22.04\home\<user>\projects\Financial-Tracker\data\inbox`).
3. Run:

```bash
docker compose up -d ollama
docker compose run --rm app import data/inbox
docker compose run --rm app summary
```

Importing the same file twice is safe: transactions already in the database are skipped.

## Fixing categories

The AI picks from the `categories` list in `rules.toml`. When it gets something
wrong, add a rule (a regex on the description) to `rules.toml` and run:

```bash
docker compose run --rm app recategorize
```

Other options: `--no-ai` (rules only), `--model qwen2.5:3b` (another Ollama model),
`summary --months 6`.

## Data

`data/` holds the statements and `finance.duckdb`. It is in `.gitignore` and must
never be committed. Back it up encrypted if you back it up at all.
