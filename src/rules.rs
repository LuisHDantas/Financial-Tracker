//! rules.toml: the category list (the only categories anything may use) and the
//! regex rules that run before the AI.

use std::path::Path;

use anyhow::{bail, Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// Used when no rule matches and the AI has no usable answer. Must be in `categories`.
pub const FALLBACK: &str = "Other";

/// Money moved between your own accounts (including paying the card bill). The
/// summary keeps it out of income and spending.
pub const TRANSFERS: &str = "Transfers";

#[derive(Deserialize)]
struct RulesFile {
    categories: Vec<String>,
    #[serde(default)]
    rules: Vec<RuleDef>,
}

#[derive(Deserialize)]
struct RuleDef {
    pattern: String,
    category: String,
}

#[derive(Debug)]
pub struct Rules {
    pub categories: Vec<String>,
    rules: Vec<(Regex, String)>,
}

impl Rules {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: RulesFile = toml::from_str(text)?;
        if !file.categories.iter().any(|c| c == FALLBACK) {
            bail!("`categories` must include \"{FALLBACK}\", used when nothing else fits");
        }
        let mut rules = Vec::new();
        for (i, def) in file.rules.into_iter().enumerate() {
            if !file.categories.contains(&def.category) {
                bail!(
                    "rule {} ({:?}) points to category {:?}, which is not in `categories`",
                    i + 1,
                    def.pattern,
                    def.category
                );
            }
            let re = RegexBuilder::new(&def.pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("rule {} has an invalid pattern {:?}", i + 1, def.pattern))?;
            rules.push((re, def.category));
        }
        Ok(Self { categories: file.categories, rules })
    }

    /// Category of the first rule that matches, top to bottom.
    pub fn category_for(&self, memo: &str) -> Option<&str> {
        self.rules.iter().find(|(re, _)| re.is_match(memo)).map(|(_, c)| c.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"
categories = ["Delivery", "Utilities", "Fees", "Other"]

[[rules]]
pattern = "IFOOD|RAPPI"
category = "Delivery"

[[rules]]
pattern = "^TAR\\b|IOF"
category = "Fees"

[[rules]]
pattern = "\\bTIM\\b|\\bOI\\b"
category = "Utilities"
"#;

    #[test]
    fn the_projects_rules_file_is_valid() {
        Rules::parse(include_str!("../rules.toml")).unwrap();
    }

    #[test]
    fn matches_case_insensitive_first_rule_wins() {
        let rules = Rules::parse(RULES).unwrap();
        assert_eq!(rules.category_for("ifood *rest"), Some("Delivery"));
        assert_eq!(rules.category_for("TAR PACOTE ITAU"), Some("Fees"));
        assert_eq!(rules.category_for("IFOOD IOF"), Some("Delivery"));
        assert_eq!(rules.category_for("PIX TRANSF JOAO"), None);
    }

    #[test]
    fn word_boundaries_avoid_false_matches() {
        let rules = Rules::parse(RULES).unwrap();
        assert_eq!(rules.category_for("TIM CELULAR"), Some("Utilities"));
        assert_eq!(rules.category_for("TIMBRE LTDA"), None);
        assert_eq!(rules.category_for("BOITATA"), None);
        assert_eq!(rules.category_for("ESTAR BEM"), None);
    }

    #[test]
    fn rejects_a_rule_with_an_unknown_category() {
        let err = Rules::parse(&RULES.replace("category = \"Fees\"", "category = \"Bank fees\"")).unwrap_err();
        assert!(err.to_string().contains("Bank fees"), "{err}");
    }

    #[test]
    fn rejects_an_invalid_pattern() {
        assert!(Rules::parse(&RULES.replace("IFOOD|RAPPI", "IFOOD(")).is_err());
    }

    #[test]
    fn requires_the_fallback_category() {
        assert!(Rules::parse(&RULES.replace(", \"Other\"", "")).is_err());
    }
}
