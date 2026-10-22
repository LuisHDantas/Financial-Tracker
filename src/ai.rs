//! Asking a local model (through Ollama) for a category.
//!
//! Ollama's `/api/chat` accepts a JSON schema in `format` and makes the model's
//! answer fit it. The schema here says "an object with a `category` that is one of
//! these strings", so the answer is always one of the categories in rules.toml (or
//! we treat it as no answer).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::money::Money;

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
    pub fn categorize(&self, memo: &str, amount: Money, categories: &[String]) -> Result<Option<String>> {
        let body = json!({
            "model": self.model,
            "stream": false,
            "options": {"temperature": 0},
            "messages": [{"role": "user", "content": prompt(memo, amount)}],
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

fn prompt(memo: &str, amount: Money) -> String {
    let direction = if amount.is_income() { "money in" } else { "money out" };
    format!(
        "Categorize this Brazilian bank transaction from an Itaú statement.\n\
         Description: {memo}\nAmount: R$ {amount} ({direction})\n\
         \"Transfers\" means moving money between the person's own accounts or paying \
         their own credit card bill; a Pix or payment to another person or a business \
         is not a transfer.\n\
         Answer with exactly one category from the list."
    )
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

    #[test]
    fn prompt_describes_the_transaction() {
        let p = prompt("IFOOD *REST", Money(-5890));
        assert!(p.contains("Description: IFOOD *REST"));
        assert!(p.contains("Amount: R$ -58.90 (money out)"));
        assert!(prompt("SALARIO", Money(100)).contains("(money in)"));
    }

    #[test]
    fn reads_the_category_out_of_the_response() {
        let ok = json!({"message": {"content": "{\"category\": \"Delivery\"}"}});
        assert_eq!(category_from_response(&ok).as_deref(), Some("Delivery"));
        let garbled = json!({"message": {"content": "Delivery"}});
        assert_eq!(category_from_response(&garbled), None);
    }
}
