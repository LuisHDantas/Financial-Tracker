//! The events: the source of truth for all data.
//!
//! This app uses *event sourcing*. Instead of editing rows in place, every change
//! is written down as an event ("this statement was imported", "this transaction
//! was seen", "this transaction got this category") and appended to the `events`
//! table, which is never changed or deleted. The tables we actually query
//! (`statements`, `transactions`) are *projections*: they are computed from the
//! events, and `finance rebuild` can throw them away and recompute them by
//! replaying the log from the start.
//!
//! Why: we keep the full history (e.g. what the AI first said before a rule fixed
//! it), and if a projection gets a bug, fixing the code and rebuilding repairs it.

use serde::{Deserialize, Serialize};

use crate::money::Money;

/// Something that happened. Stored as JSON, e.g.
/// `{"type":"TransactionCategorized","id":"...","category":"Delivery","source":"rule","model":null}`.
///
/// Field names are part of the stored format: renaming one breaks reading old events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// A statement file was read. Its sha256 stops the same file being imported twice.
    StatementImported { file_name: String, sha256: String, account: String, txn_count: i64 },
    /// A transaction was seen for the first time.
    TransactionObserved {
        id: String,
        account: String,
        account_type: AccountType,
        fitid: String,
        date: String, // YYYY-MM-DD
        amount_cents: Money,
        memo: String,
        trn_type: Option<String>,
        source_file: String,
    },
    /// A transaction got a new category (or a new source for it).
    TransactionCategorized { id: String, category: String, source: CategorySource, model: Option<String> },
}

impl Event {
    /// The event's name, stored in its own column so the log is easy to filter.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::StatementImported { .. } => "StatementImported",
            Self::TransactionObserved { .. } => "TransactionObserved",
            Self::TransactionCategorized { .. } => "TransactionCategorized",
        }
    }
}

/// Which kind of statement a transaction came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    Checking,
    CreditCard,
}

impl AccountType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::CreditCard => "credit_card",
        }
    }
}

/// Who chose a transaction's category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CategorySource {
    /// A rule in rules.toml matched.
    Rule,
    /// The local model picked it.
    Ai,
    /// Nothing did; the fallback category was used.
    None,
}

impl CategorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Ai => "ai",
            Self::None => "none",
        }
    }

    /// The reverse of `as_str`, for reading the `category_source` column.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "rule" => Some(Self::Rule),
            "ai" => Some(Self::Ai),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_json() {
        let event = Event::TransactionCategorized {
            id: "abc".into(),
            category: "Delivery".into(),
            source: CategorySource::Ai,
            model: Some("llama3.2:3b".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"TransactionCategorized""#));
        assert!(json.contains(r#""source":"ai""#));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn amounts_are_stored_as_plain_cents() {
        let event = Event::TransactionObserved {
            id: "x".into(),
            account: "1".into(),
            account_type: AccountType::CreditCard,
            fitid: "f".into(),
            date: "2026-10-03".into(),
            amount_cents: Money(-12_000),
            memo: "RESTAURANTE".into(),
            trn_type: None,
            source_file: "card.ofx".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""amount_cents":-12000"#), "{json}");
        assert!(json.contains(r#""account_type":"credit_card""#), "{json}");
    }

    #[test]
    fn category_source_parses_its_own_names() {
        for source in [CategorySource::Rule, CategorySource::Ai, CategorySource::None] {
            assert_eq!(CategorySource::parse(source.as_str()), Some(source));
        }
        assert_eq!(CategorySource::parse("other"), None);
    }
}
