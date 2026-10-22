//! Personal finance tracker.
//!
//! Reads Itaú OFX statements (checking and credit card), records everything as
//! events in DuckDB, categorizes transactions (rules.toml first, then a local model
//! through Ollama) and prints a monthly summary.
//!
//! How the code is laid out:
//! - `main.rs`: the command line (clap) and nothing else
//! - `import.rs`, `categorize.rs`, `summary.rs`: one file per command's work
//! - `ofx.rs`: reading statement files
//! - `rules.rs`, `ai.rs`: the two ways of choosing a category
//! - `events.rs`, `store.rs`: the event log and the tables built from it
//! - `money.rs`: amounts in cents

mod ai;
mod categorize;
mod events;
mod import;
mod money;
mod ofx;
mod rules;
mod store;
mod summary;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::ai::Ollama;
use crate::categorize::Scope;
use crate::rules::Rules;

#[derive(Parser)]
#[command(
    name = "finance",
    about = "Personal finance tracker: Itaú OFX statements, an event log in DuckDB, local AI categorization"
)]
struct Cli {
    /// Folder holding finance.duckdb
    #[arg(long, env = "FINANCE_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    /// Categories and rules file
    #[arg(long, env = "FINANCE_RULES", default_value = "rules.toml", global = true)]
    rules: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import an .ofx file or a folder of them
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
    let con = store::open(&cli.data_dir)?;
    match cli.command {
        Command::Import { path, ai } => {
            let rules = Rules::load(&cli.rules)?; // fail on a bad rules file before writing anything
            import::run(&con, &path)?;
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
    }
    Ok(())
}
