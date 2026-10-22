//! Categorization by a local model through Ollama's /api/chat. The answer is
//! constrained by a JSON schema to one of the categories in rules.toml.

use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

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

    /// Ok(None) when the model answered something unusable; Err when Ollama can't be
    /// reached or refuses (not running, model not pulled).
    pub fn categorize(&self, memo: &str, amount_cents: i64, categories: &[String]) -> Result<Option<String>> {
        let direction = if amount_cents > 0 { "money in" } else { "money out" };
        let prompt = format!(
            "Categorize this Brazilian bank transaction from an Itaú statement.\n\
             Description: {memo}\nAmount: R$ {} ({direction})\n\
             \"Transfers\" means moving money between the person's own accounts or paying \
             their own credit card bill; a Pix or payment to another person or a business \
             is not a transfer.\n\
             Answer with exactly one category from the list.",
            crate::money(amount_cents)
        );
        let body = json!({
            "model": self.model,
            "stream": false,
            "options": {"temperature": 0},
            "messages": [{"role": "user", "content": prompt}],
            "format": {
                "type": "object",
                "properties": {"category": {"type": "string", "enum": categories}},
                "required": ["category"],
            },
        });
        let response: Value = self.agent.post(&self.url).send_json(body)?.into_json()?;
        let category = response["message"]["content"]
            .as_str()
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .and_then(|answer| answer["category"].as_str().map(str::to_owned));
        Ok(category.filter(|c| categories.contains(c)))
    }
}
