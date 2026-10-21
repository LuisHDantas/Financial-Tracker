# /// script
# requires-python = ">=3.11"
# dependencies = ["duckdb>=1.1"]
# ///
"""Personal finance tracker, v0.

Reads Itaú OFX statements, skips transactions it already has, categorizes them
(rules first, then a local model through Ollama), stores everything in DuckDB,
and prints a monthly summary.

    uv run finance.py import data/inbox       # an .ofx file or a folder of them
    uv run finance.py summary                 # spending by category per month
    uv run finance.py recategorize            # re-apply rules.toml (and AI) to everything
"""

import argparse
import hashlib
import json
import os
import re
import sys
import tomllib
import urllib.error
import urllib.request
from datetime import date
from pathlib import Path

import duckdb

HERE = Path(__file__).parent
DATA_DIR = Path(os.environ.get("FINANCE_DATA_DIR", HERE / "data"))
DB_PATH = DATA_DIR / "finance.duckdb"
RULES_PATH = HERE / "rules.toml"
OLLAMA_URL = os.environ.get("OLLAMA_HOST", "http://localhost:11434").rstrip("/") + "/api/chat"
DEFAULT_MODEL = os.environ.get("OLLAMA_MODEL", "llama3.2:3b")
FALLBACK_CATEGORY = "Other"

SCHEMA = """
CREATE TABLE IF NOT EXISTS transactions (
    id              VARCHAR PRIMARY KEY,   -- hash of account + FITID + date + amount + memo
    account         VARCHAR,
    fitid           VARCHAR,
    date            DATE,
    amount          DECIMAL(14, 2),        -- negative = money out
    memo            VARCHAR,
    trn_type        VARCHAR,
    source_file     VARCHAR,
    imported_at     TIMESTAMP DEFAULT current_timestamp,
    category        VARCHAR,
    category_source VARCHAR                -- 'rule', 'ai' or 'none'
);
CREATE TABLE IF NOT EXISTS ai_cache (
    memo_key   VARCHAR PRIMARY KEY,        -- memo without digits, so similar memos share an answer
    category   VARCHAR,
    model      VARCHAR,
    created_at TIMESTAMP DEFAULT current_timestamp
);
"""


# --- OFX parsing -----------------------------------------------------------

def read_ofx_text(path: Path) -> str:
    raw = path.read_bytes()
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return raw.decode("cp1252")  # Brazilian banks often export Latin-1/Windows-1252


def tag(block: str, name: str) -> str | None:
    # Works for both SGML OFX (no closing tags) and XML OFX.
    m = re.search(rf"<{name}>([^<\r\n]*)", block, re.IGNORECASE)
    return m.group(1).strip() if m else None


def parse_amount(text: str) -> float:
    text = text.strip()
    if "," in text and "." not in text:
        text = text.replace(",", ".")
    return round(float(text), 2)


def parse_ofx(path: Path) -> list[dict]:
    text = read_ofx_text(path)
    account = tag(text, "ACCTID") or "unknown"
    rows = []
    for block in re.findall(r"<STMTTRN>(.*?)(?=<STMTTRN>|</BANKTRANLIST>|</CCSTMTRS>|$)", text, re.S | re.I):
        posted = tag(block, "DTPOSTED")
        amount = tag(block, "TRNAMT")
        if not posted or not amount:
            continue
        d = date(int(posted[:4]), int(posted[4:6]), int(posted[6:8]))
        memo = tag(block, "MEMO") or tag(block, "NAME") or ""
        fitid = tag(block, "FITID") or ""
        amt = parse_amount(amount)
        key = f"{account}|{fitid}|{d.isoformat()}|{amt:.2f}|{memo}"
        rows.append({
            "id": hashlib.sha256(key.encode()).hexdigest()[:24],
            "account": account,
            "fitid": fitid,
            "date": d,
            "amount": amt,
            "memo": re.sub(r"\s+", " ", memo),
            "trn_type": tag(block, "TRNTYPE"),
        })
    return rows


# --- Categorization --------------------------------------------------------

def load_rules() -> tuple[list[str], list[tuple[re.Pattern, str]]]:
    config = tomllib.loads(RULES_PATH.read_text(encoding="utf-8"))
    categories = config["categories"]
    rules = [(re.compile(r["pattern"], re.I), r["category"]) for r in config.get("rules", [])]
    return categories, rules


def memo_key(memo: str) -> str:
    return re.sub(r"\s+", " ", re.sub(r"[\d/.\-:]+", " ", memo.upper())).strip()


def ask_ollama(memo: str, amount: float, categories: list[str], model: str) -> str | None:
    prompt = (
        "Categorize this Brazilian bank transaction from an Itaú statement.\n"
        f"Description: {memo}\nAmount: R$ {amount:.2f} "
        f"({'money in' if amount > 0 else 'money out'})\n"
        "Answer with exactly one category from the list."
    )
    body = {
        "model": model,
        "stream": False,
        "options": {"temperature": 0},
        "messages": [{"role": "user", "content": prompt}],
        "format": {
            "type": "object",
            "properties": {"category": {"type": "string", "enum": categories}},
            "required": ["category"],
        },
    }
    req = urllib.request.Request(OLLAMA_URL, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        content = json.loads(resp.read())["message"]["content"]
    category = json.loads(content).get("category")
    return category if category in categories else None


def categorize(con, only_missing: bool, use_ai: bool, model: str) -> None:
    categories, rules = load_rules()
    where = "WHERE category IS NULL OR category_source = 'none'" if only_missing else ""
    rows = con.execute(f"SELECT id, memo, amount FROM transactions {where}").fetchall()

    ai_ok = use_ai
    by_rule = by_ai = left = 0
    for tid, memo, amount in rows:
        amount = float(amount)
        category = next((cat for pattern, cat in rules if pattern.search(memo)), None)
        source = "rule"
        if category is None and ai_ok:
            key = memo_key(memo)
            cached = con.execute("SELECT category FROM ai_cache WHERE memo_key = ?", [key]).fetchone()
            if cached:
                category = cached[0]
            else:
                try:
                    category = ask_ollama(memo, amount, categories, model)
                except (urllib.error.URLError, TimeoutError) as e:
                    print(f"Ollama not reachable ({e}); skipping AI for the rest. Is `ollama serve` running?")
                    ai_ok = False
                if category:
                    con.execute("INSERT OR REPLACE INTO ai_cache (memo_key, category, model) VALUES (?, ?, ?)",
                                [key, category, model])
            source = "ai"
        if category is None:
            category, source = FALLBACK_CATEGORY, "none"
        con.execute("UPDATE transactions SET category = ?, category_source = ? WHERE id = ?",
                    [category, source, tid])
        by_rule += source == "rule"
        by_ai += source == "ai"
        left += source == "none"
    print(f"Categorized {len(rows)} transactions: {by_rule} by rules, {by_ai} by AI, {left} left as '{FALLBACK_CATEGORY}'.")


# --- Commands --------------------------------------------------------------

def connect():
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    con = duckdb.connect(str(DB_PATH))
    con.execute(SCHEMA)
    return con


def cmd_import(args) -> None:
    target = Path(args.path)
    files = sorted(target.glob("*.ofx")) + sorted(target.glob("*.OFX")) if target.is_dir() else [target]
    con = connect()
    for f in files:
        rows = parse_ofx(f)
        before = con.execute("SELECT count(*) FROM transactions").fetchone()[0]
        for r in rows:
            con.execute(
                "INSERT OR IGNORE INTO transactions (id, account, fitid, date, amount, memo, trn_type, source_file) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                [r["id"], r["account"], r["fitid"], r["date"], r["amount"], r["memo"], r["trn_type"], f.name],
            )
        added = con.execute("SELECT count(*) FROM transactions").fetchone()[0] - before
        print(f"{f.name}: {len(rows)} transactions in file, {added} new, {len(rows) - added} already imported.")
    categorize(con, only_missing=True, use_ai=not args.no_ai, model=args.model)


def cmd_recategorize(args) -> None:
    con = connect()
    if args.clear_ai_cache:
        con.execute("DELETE FROM ai_cache")
    categorize(con, only_missing=False, use_ai=not args.no_ai, model=args.model)


def cmd_summary(args) -> None:
    con = connect()
    months = con.execute(
        "SELECT DISTINCT strftime(date, '%Y-%m') AS m FROM transactions ORDER BY m DESC LIMIT ?",
        [args.months],
    ).fetchall()
    for (month,) in reversed(months):
        income, spent = con.execute(
            "SELECT coalesce(sum(amount) FILTER (WHERE amount > 0), 0), "
            "       coalesce(-sum(amount) FILTER (WHERE amount < 0), 0) "
            "FROM transactions WHERE strftime(date, '%Y-%m') = ?", [month]).fetchone()
        print(f"\n=== {month} ===   in: R$ {income:,.2f}   out: R$ {spent:,.2f}   net: R$ {income - spent:,.2f}")
        for category, total, n in con.execute(
            "SELECT category, -sum(amount) AS total, count(*) FROM transactions "
            "WHERE amount < 0 AND strftime(date, '%Y-%m') = ? "
            "GROUP BY category ORDER BY total DESC", [month]).fetchall():
            share = total / spent * 100 if spent else 0
            print(f"  {category:<16} R$ {total:>10,.2f}  {share:5.1f}%  ({n} txns)")
    unsure = con.execute(
        "SELECT date, amount, memo FROM transactions WHERE category_source = 'none' ORDER BY date DESC LIMIT 10"
    ).fetchall()
    if unsure:
        print(f"\nStill uncategorized (add a rule to rules.toml, then run `recategorize`):")
        for d, amount, memo in unsure:
            print(f"  {d}  R$ {float(amount):>10,.2f}  {memo}")


def main() -> None:
    parser = argparse.ArgumentParser(description="Personal finance tracker (Itaú OFX + DuckDB + Ollama)")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("import", help="import an .ofx file or a folder of them")
    p.add_argument("path")
    p.set_defaults(func=cmd_import)

    p = sub.add_parser("recategorize", help="re-apply rules and AI to all transactions")
    p.add_argument("--clear-ai-cache", action="store_true", help="ask the model again for every memo")
    p.set_defaults(func=cmd_recategorize)

    for p in (sub.choices["import"], sub.choices["recategorize"]):
        p.add_argument("--no-ai", action="store_true", help="rules only, don't call Ollama")
        p.add_argument("--model", default=DEFAULT_MODEL, help=f"Ollama model (default {DEFAULT_MODEL})")

    p = sub.add_parser("summary", help="spending by category per month")
    p.add_argument("--months", type=int, default=3)
    p.set_defaults(func=cmd_summary)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    sys.exit(main())
