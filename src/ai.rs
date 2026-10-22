//! Asking a local model (through Ollama) for a category.
//!
//! Ollama's `/api/chat` accepts a JSON schema in `format` and makes the model's
//! answer fit it. The schema here says "an object with a `category` that is one of
//! these strings", so the answer is always one of the categories in rules.toml (or
//! we treat it as no answer).
//!
//! The schema only limits *what* the model may answer; it doesn't tell the model
//! what the options are. So the prompt also lists the categories with a one-line
//! meaning each and shows the examples from rules.toml. `finance bench` measures
//! the effect: on the test cases the AI alone went from 46% right (categories and
//! examples, no meanings) to 68% (with meanings).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::money::Money;
use crate::rules::Rules;

/// A connection to one model on an Ollama server.
pub struct Ollama {
    agent: ureq::Agent,
    url: String,
    pub model: String,
}

impl Ollama {
    pub fn new(host: &str, model: &str) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).build(),
            url: format!("{}/api/chat", host.trim_end_matches('/')),
            model: model.to_string(),
        }
    }

    /// The model's category for one transaction. `Ok(None)` when it answered
    /// something unusable; `Err` when Ollama can't be reached or refuses (not
    /// running, model not pulled).
    pub fn categorize(&self, memo: &str, amount: Money, rules: &Rules) -> Result<Option<String>> {
        let categories = &rules.categories;
        let body = json!({
            "model": self.model,
            "stream": false,
            "options": {"temperature": 0},
            "messages": [{"role": "user", "content": prompt(memo, amount, rules)}],
            "format": {
                "type": "object",
                "properties": {"category": {"type": "string", "enum": categories}},
                "required": ["category"],
            },
        });
        let response: Value = self
            .agent
            .post(&self.url)
            .send_json(body)
            .with_context(|| format!("calling {}", self.url))?
            .into_json()
            .context("reading Ollama's answer")?;
        Ok(category_from_response(&response).filter(|c| categories.contains(c)))
    }
}

/// The question for one transaction: the categories with their meanings and the
/// examples from rules.toml, then the transaction itself.
fn prompt(memo: &str, amount: Money, rules: &Rules) -> String {
    let mut text = String::from("Categorize this Brazilian bank transaction from an Itaú statement.\n");
    text += "Categories:\n";
    for category in &rules.categories {
        match rules.meanings.get(category) {
            Some(meaning) => text += &format!("- {category}: {meaning}\n"),
            None => text += &format!("- {category}\n"),
        }
    }
    if !rules.examples.is_empty() {
        text += "Examples:\n";
        for example in &rules.examples {
            text += &format!("- {} -> {}\n", example.description, example.category);
        }
    }
    let direction = if amount.is_income() { "money in" } else { "money out" };
    text += &format!("Description: {memo}\nAmount: R$ {amount} ({direction})\n");
    text += "Answer with exactly one category from the list.";
    text
}

/// Ollama returns `{"message": {"content": "{\"category\": \"Delivery\"}"}}`:
/// the answer is JSON inside a string.
fn category_from_response(response: &Value) -> Option<String> {
    let content = response["message"]["content"].as_str()?;
    let answer: Value = serde_json::from_str(content).ok()?;
    answer["category"].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"
categories = ["Delivery", "Transfers", "Other"]

[meanings]
Transfers = "only moving money between your own accounts"

[[examples]]
description = "TED PARA CONTA PROPRIA"
category = "Transfers"
"#;

    #[test]
    fn prompt_describes_the_transaction() {
        let rules = Rules::parse(RULES).unwrap();
        let p = prompt("IFOOD *REST", Money(-5890), &rules);
        assert!(p.contains("Description: IFOOD *REST"));
        assert!(p.contains("Amount: R$ -58.90 (money out)"));
        assert!(prompt("SALARIO", Money(100), &rules).contains("(money in)"));
    }

    #[test]
    fn prompt_lists_categories_meanings_and_examples() {
        let rules = Rules::parse(RULES).unwrap();
        let p = prompt("IFOOD *REST", Money(-5890), &rules);
        assert!(
            p.contains(
                "Categories:\n- Delivery\n- Transfers: only moving money between your own accounts\n- Other\n"
            ),
            "{p}"
        );
        assert!(p.contains("Examples:\n- TED PARA CONTA PROPRIA -> Transfers\n"), "{p}");
        // The transaction comes after the examples, so the model doesn't mistake one for the other.
        assert!(p.find("Examples:").unwrap() < p.find("Description: IFOOD").unwrap());
    }

    #[test]
    fn prompt_leaves_out_what_rules_toml_doesnt_have() {
        let rules = Rules::parse("categories = [\"Delivery\", \"Other\"]").unwrap();
        let p = prompt("IFOOD *REST", Money(-5890), &rules);
        assert!(p.contains("Categories:\n- Delivery\n- Other\n"), "{p}");
        assert!(!p.contains("Examples:"), "{p}");
    }

    #[test]
    fn reads_the_category_out_of_the_response() {
        let ok = json!({"message": {"content": "{\"category\": \"Delivery\"}"}});
        assert_eq!(category_from_response(&ok).as_deref(), Some("Delivery"));
        let garbled = json!({"message": {"content": "Delivery"}});
        assert_eq!(category_from_response(&garbled), None);
    }
}
