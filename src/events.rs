//! The event log is the source of truth. Every change is appended as one of these
//! events; the `statements` and `transactions` tables are projections of it and can
//! be rebuilt at any time with `finance rebuild`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// A statement file was read. Its sha256 stops the same file being imported twice.
    StatementImported {
        file_name: String,
        sha256: String,
        account: String,
        txn_count: i64,
    },
    /// A transaction was seen for the first time.
    TransactionObserved {
        id: String,
        account: String,
        account_type: AccountType,
        fitid: String,
        date: String, // YYYY-MM-DD
        amount_cents: i64, // negative = money out
        memo: String,
        trn_type: Option<String>,
        source_file: String,
    },
    /// A transaction got a (new) category.
    TransactionCategorized {
        id: String,
        category: String,
        source: CategorySource,
        model: Option<String>,
    },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::StatementImported { .. } => "StatementImported",
            Self::TransactionObserved { .. } => "TransactionObserved",
            Self::TransactionCategorized { .. } => "TransactionCategorized",
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CategorySource {
    Rule,
    Ai,
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
}
