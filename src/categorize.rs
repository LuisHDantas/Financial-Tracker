//! Choosing a category for each transaction.
//!
//! For each transaction, in order: the first matching rule in rules.toml; else the
//! AI (reusing an earlier answer for a similar memo when there is one); else an
//! earlier AI answer for this same transaction, if the AI isn't available now;
//! else the fallback category. A `TransactionCategorized` event is appended only
//! when the result differs from what the transaction already has, so running this
//! again doesn't fill the log with repeats.

use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use duckdb::Connection;

use crate::ai::Ollama;
use crate::events::{CategorySource, Event};
use crate::rules::{Rules, FALLBACK};
use crate::store::{self, TransactionRow};

/// Which transactions to look at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only new ones and ones left in the fallback category (after an import).
    Uncategorized,
    /// All of them (after editing rules.toml).
    All,
}

/// Categorizes the transactions in `scope` and records what changed.
pub fn run(
    con: &Connection,
    rules: &Rules,
    ai: Option<Ollama>,
    scope: Scope,
    reuse_ai_answers: bool,
) -> Result<Tally> {
    let rows = store::transactions_to_categorize(con, scope == Scope::Uncategorized)?;
    let mut categorizer = Categorizer { con, rules, ai, reuse_ai_answers, answered: HashMap::new() };
    let mut tally = Tally { total: rows.len(), ..Tally::default() };
    for row in &rows {
        let decision = categorizer.decide(row)?;
        tally.count(decision.source);
        if decision.differs_from(row) {
            record(con, row, decision)?;
            tally.changed += 1;
        }
    }
    Ok(tally)
}

fn record(con: &Connection, row: &TransactionRow, decision: Decision) -> Result<()> {
    let event = Event::TransactionCategorized {
        id: row.id.clone(),
        category: decision.category,
        source: decision.source,
        model: decision.model,
    };
    store::in_transaction(con, |con| store::append(con, &event))?;
    Ok(())
}

/// A category and who chose it.
struct Decision {
    category: String,
    source: CategorySource,
    model: Option<String>,
}

impl Decision {
    fn fallback() -> Self {
        Self { category: FALLBACK.to_string(), source: CategorySource::None, model: None }
    }

    fn differs_from(&self, row: &TransactionRow) -> bool {
        row.category.as_deref() != Some(self.category.as_str()) || row.category_source != Some(self.source)
    }
}

struct Categorizer<'a> {
    con: &'a Connection,
    rules: &'a Rules,
    /// `None` with --no-ai, or after Ollama failed once in this run.
    ai: Option<Ollama>,
    /// False with --clear-ai-cache: ask the model again instead of reusing old answers.
    reuse_ai_answers: bool,
    /// memo_key -> category, for answers the model gave during this run.
    answered: HashMap<String, String>,
}

impl Categorizer<'_> {
    fn decide(&mut self, row: &TransactionRow) -> Result<Decision> {
        if let Some(decision) = self.by_rule(row) {
            return Ok(decision);
        }
        if let Some(decision) = self.by_ai(row)? {
            return Ok(decision);
        }
        Ok(keep_earlier_ai_answer(row).unwrap_or_else(Decision::fallback))
    }

    fn by_rule(&self, row: &TransactionRow) -> Option<Decision> {
        let category = self.rules.category_for(&row.memo)?;
        Some(Decision { category: category.to_string(), source: CategorySource::Rule, model: None })
    }

    fn by_ai(&mut self, row: &TransactionRow) -> Result<Option<Decision>> {
        let Some(client) = &self.ai else {
            return Ok(None);
        };
        let model = client.model.clone();
        let category = match self.earlier_answer(&row.memo_key)? {
            Some(category) => Some(category),
            None => match client.categorize(&row.memo, row.amount, &self.rules.categories) {
                Ok(answer) => answer,
                Err(e) => {
                    println!("Ollama failed ({e:#}); skipping AI for the rest of this run. Is the ollama container running?");
                    self.ai = None;
                    return Ok(None);
                }
            },
        };
        if let Some(category) = &category {
            self.answered.insert(row.memo_key.clone(), category.clone());
        }
        Ok(category.map(|category| Decision { category, source: CategorySource::Ai, model: Some(model) }))
    }

    /// An answer the model already gave for a similar memo, in this run or before.
    fn earlier_answer(&self, memo_key: &str) -> Result<Option<String>> {
        if let Some(category) = self.answered.get(memo_key) {
            return Ok(Some(category.clone()));
        }
        if !self.reuse_ai_answers {
            return Ok(None);
        }
        store::earlier_ai_answer(self.con, memo_key)
    }
}

/// Without a rule or a new AI answer, keep the AI's earlier answer rather than
/// dropping the transaction to the fallback category.
fn keep_earlier_ai_answer(row: &TransactionRow) -> Option<Decision> {
    if row.category_source != Some(CategorySource::Ai) {
        return None;
    }
    Some(Decision { category: row.category.clone()?, source: CategorySource::Ai, model: row.model.clone() })
}

/// How many transactions each step categorized.
#[derive(Default, Debug, PartialEq)]
pub struct Tally {
    pub total: usize,
    pub by_rule: usize,
    pub by_ai: usize,
    pub left: usize,
    pub changed: usize,
}

impl Tally {
    fn count(&mut self, source: CategorySource) {
        match source {
            CategorySource::Rule => self.by_rule += 1,
            CategorySource::Ai => self.by_ai += 1,
            CategorySource::None => self.left += 1,
        }
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Categorized {} transactions: {} by rules, {} by AI, {} left as '{FALLBACK}' ({} changed).",
            self.total, self.by_rule, self.by_ai, self.left, self.changed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AccountType;
    use crate::money::Money;

    const RULES: &str = r#"
categories = ["Delivery", "Shopping", "Other"]

[[rules]]
pattern = "IFOOD"
category = "Delivery"
"#;

    fn setup(memos: &[&str]) -> Connection {
        let con = store::open_in_memory().unwrap();
        store::in_transaction(&con, |con| {
            for (i, memo) in memos.iter().enumerate() {
                store::append(
                    con,
                    &Event::TransactionObserved {
                        id: format!("t{i}"),
                        account: "1".into(),
                        account_type: AccountType::Checking,
                        fitid: format!("f{i}"),
                        date: "2026-10-05".into(),
                        amount_cents: Money(-1000),
                        memo: memo.to_string(),
                        trn_type: None,
                        source_file: "test.ofx".into(),
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();
        con
    }

    fn categories(con: &Connection) -> Vec<(String, Option<CategorySource>)> {
        store::transactions_to_categorize(con, false)
            .unwrap()
            .into_iter()
            .map(|r| (r.category.unwrap_or_default(), r.category_source))
            .collect()
    }

    #[test]
    fn rules_first_then_fallback_and_no_repeat_events() {
        let con = setup(&["IFOOD *REST", "LIVRARIA CULTURA"]);
        let rules = Rules::parse(RULES).unwrap();

        let first = run(&con, &rules, None, Scope::Uncategorized, true).unwrap();
        assert_eq!(first, Tally { total: 2, by_rule: 1, by_ai: 0, left: 1, changed: 2 });
        assert_eq!(
            categories(&con),
            vec![
                ("Delivery".into(), Some(CategorySource::Rule)),
                ("Other".into(), Some(CategorySource::None))
            ]
        );

        let again = run(&con, &rules, None, Scope::All, true).unwrap();
        assert_eq!(again.changed, 0);
    }

    #[test]
    fn keeps_an_earlier_ai_answer_when_the_ai_is_off() {
        let con = setup(&["LIVRARIA CULTURA"]);
        store::in_transaction(&con, |con| {
            store::append(
                con,
                &Event::TransactionCategorized {
                    id: "t0".into(),
                    category: "Shopping".into(),
                    source: CategorySource::Ai,
                    model: Some("m".into()),
                },
            )
        })
        .unwrap();
        let rules = Rules::parse(RULES).unwrap();
        let tally = run(&con, &rules, None, Scope::All, true).unwrap();
        assert_eq!((tally.by_ai, tally.changed), (1, 0));
        assert_eq!(categories(&con), vec![("Shopping".into(), Some(CategorySource::Ai))]);
    }

    #[test]
    fn a_new_rule_overrides_an_ai_answer() {
        let con = setup(&["LIVRARIA CULTURA"]);
        store::in_transaction(&con, |con| {
            store::append(
                con,
                &Event::TransactionCategorized {
                    id: "t0".into(),
                    category: "Delivery".into(),
                    source: CategorySource::Ai,
                    model: Some("m".into()),
                },
            )
        })
        .unwrap();
        let rules =
            Rules::parse(&format!("{RULES}\n[[rules]]\npattern = \"LIVRARIA\"\ncategory = \"Shopping\"\n"))
                .unwrap();
        let tally = run(&con, &rules, None, Scope::All, true).unwrap();
        assert_eq!((tally.by_rule, tally.changed), (1, 1));
        assert_eq!(categories(&con), vec![("Shopping".into(), Some(CategorySource::Rule))]);
    }
}
