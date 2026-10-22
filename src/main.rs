//! Personal finance tracker.
//!
//! Reads Itaú OFX statements (checking and credit card), records everything as
//! events in DuckDB, categorizes transactions (rules.toml first, then a local model
//! through Ollama) and prints a monthly summary.
//!
//! How the code is laid out:
//! - `main.rs`: the command line (clap) and nothing else
//! - `import.rs`, `categorize.rs`, `summary.rs`: one file per command's work
//! - `ofx.rs`, `pdf.rs`: reading statement files (PDF layouts are in parsers.toml)
//! - `rules.rs`, `ai.rs`: the two ways of choosing a category
//! - `events.rs`, `store.rs`: the event log and the tables built from it
//! - `money.rs`: amounts in cents
//! - `bench.rs`: scoring the rules and the AI on labeled descriptions
//! - `web.rs`: the web page and its JSON API (`finance serve`)

mod ai;
mod bench;
mod categorize;
mod events;
mod import;
mod money;
mod ofx;
mod pdf;
mod rules;
mod store;
mod summary;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::ai::Ollama;
use crate::categorize::Scope;
use crate::pdf::Layouts;
use crate::rules::Rules;

#[derive(Parser)]
#[command(
    name = "finance",
    about = "Personal finance tracker: Itaú statements (OFX or PDF), an event log in DuckDB, local AI categorization"
)]
struct Cli {
    /// Folder holding finance.duckdb
    #[arg(long, env = "FINANCE_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    /// Categories and rules file
    #[arg(long, env = "FINANCE_RULES", default_value = "rules.toml", global = true)]
    rules: PathBuf,
    /// PDF layouts file
    #[arg(long, env = "FINANCE_PARSERS", default_value = "parsers.toml", global = true)]
    parsers: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import an .ofx/.pdf file or a folder of them
    Import {
        path: PathBuf,
        #[command(flatten)]
        ai: AiArgs,
    },
    /// Re-apply rules.toml (and the AI) to every transaction
    Recategorize {
        /// Ask the model again instead of reusing earlier AI answers
        #[arg(long)]
        clear_ai_cache: bool,
        #[command(flatten)]
        ai: AiArgs,
    },
    /// Income and spending by category per month
    Summary {
        #[arg(long, default_value_t = 3)]
        months: i64,
    },
    /// Print the most recent events from the log
    Events {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Rebuild the statements and transactions tables by replaying every event
    Rebuild,
    /// Score the rules and the AI on labeled made-up descriptions
    Bench {
        #[arg(long, default_value = "bench/descriptions.toml")]
        file: PathBuf,
        /// Only the "dev" or "test" cases
        #[arg(long)]
        split: Option<String>,
        #[command(flatten)]
        ai: AiArgs,
    },
    /// Show a statement's line shapes (no content) to adjust parsers.toml
    Inspect { file: PathBuf },
    /// Serve the web page (overview, transactions, review, import)
    Serve {
        #[arg(long, default_value = "0.0.0.0:8080")]
        addr: SocketAddr,
        #[command(flatten)]
        ai: AiArgs,
    },
}

#[derive(Args)]
struct AiArgs {
    /// Rules only, don't call Ollama
    #[arg(long)]
    no_ai: bool,
    /// Ollama model to use
    #[arg(long, env = "OLLAMA_MODEL", default_value = "qwen2.5:3b")]
    model: String,
    #[arg(long, env = "OLLAMA_HOST", default_value = "http://localhost:11434", hide = true)]
    ollama_host: String,
}

impl AiArgs {
    fn client(&self) -> Option<Ollama> {
        (!self.no_ai).then(|| Ollama::new(&self.ollama_host, &self.model))
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Bench { file, split, ai } = &cli.command {
        // Doesn't touch the database.
        return bench::run(&Rules::load(&cli.rules)?, ai.client().as_ref(), file, split.as_deref());
    }
    if let Command::Inspect { file } = &cli.command {
        let layouts = Layouts::load(&cli.parsers)?;
        let is_pdf = file.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let text = if is_pdf {
            pdf::extract_text(file)?
        } else {
            let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
            String::from_utf8_lossy(&bytes).into_owned()
        };
        print!("{}", pdf::inspect(&text, &layouts));
        return Ok(());
    }
    if let Command::Serve { addr, ai } = &cli.command {
        // Fail at start on a bad rules or parsers file, not on the first page load.
        Rules::load(&cli.rules)?;
        Layouts::load(&cli.parsers)?;
        let config = web::Config {
            data_dir: cli.data_dir.clone(),
            rules_path: cli.rules.clone(),
            parsers_path: cli.parsers.clone(),
            ollama: (!ai.no_ai).then(|| (ai.ollama_host.clone(), ai.model.clone())),
        };
        return web::serve(*addr, config);
    }
    let con = store::open(&cli.data_dir)?;
    match cli.command {
        Command::Import { path, ai } => {
            // Fail on a bad rules or parsers file before writing anything.
            let rules = Rules::load(&cli.rules)?;
            let layouts = Layouts::load(&cli.parsers)?;
            let files = import::run(&con, &path, &layouts)?;
            if files.is_empty() {
                println!("No .ofx or .pdf files in {}.", path.display());
            }
            for file in files {
                println!("{file}");
            }
            println!("{}", categorize::run(&con, &rules, ai.client(), Scope::Uncategorized, true)?);
        }
        Command::Recategorize { clear_ai_cache, ai } => {
            let rules = Rules::load(&cli.rules)?;
            println!("{}", categorize::run(&con, &rules, ai.client(), Scope::All, !clear_ai_cache)?);
        }
        Command::Summary { months } => summary::print(&con, months)?,
        Command::Events { limit } => {
            for e in store::recent_events(&con, limit)? {
                println!("#{:<5} {}  {:<22} {}", e.seq, e.occurred_at, e.kind, e.payload);
            }
        }
        Command::Rebuild => {
            let n = store::rebuild(&con)?;
            println!("Replayed {n} events into the statements and transactions tables.");
        }
        Command::Bench { .. } | Command::Inspect { .. } | Command::Serve { .. } => {
            unreachable!("handled above")
        }
    }
    Ok(())
}
