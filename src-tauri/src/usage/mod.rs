//! Metadata-only local usage parsing and normalized event contracts.

pub mod claude;
pub mod codex;
pub mod service;

use serde::{Deserialize, Serialize};
use yaat_contracts::{Platform, TokenBreakdown, UsageDiagnostics};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageEventDraft {
    pub platform: Platform,
    pub source_event_key: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub turn_id: Option<String>,
    pub request_id: Option<String>,
    pub message_id: Option<String>,
    pub occurred_at_ms: i64,
    pub model: Option<String>,
    pub tokens: TokenBreakdown,
    pub request_count: u64,
    pub quality: UsageQuality,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuality {
    Exact,
    #[default]
    Normalized,
    Heuristic,
}

#[derive(Clone, Debug, Default)]
pub struct ParseOutcome {
    pub events: Vec<UsageEventDraft>,
    pub diagnostics: UsageDiagnostics,
}
