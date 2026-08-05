//! Full-snapshot, metadata-only parser for Claude Code transcript usage.
//!
//! Claude Code writes the same API response more than once while a streamed
//! assistant message is being assembled. It can also replay parent messages in
//! sidechain/subagent transcripts. This parser deliberately deserializes only
//! the envelope, identifiers, timestamp, model, stop marker, and usage object;
//! message content is an ignored field and is never retained by YAAT.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    path::{Component, Path},
};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use yaat_contracts::{Platform, TokenBreakdown, UsageDiagnostics};

use super::{ParseOutcome, UsageEventDraft, UsageQuality};

/// Increment whenever event-key semantics change.
pub const PARSER_VERSION: u32 = 1;

const EVENT_KEY_PREFIX: &str = "claude";

/// Stable context for one Claude Code transcript file.
///
/// `source_key` scopes globally unique Claude message/request identifiers to a
/// project without including the physical config home. Unified history may put
/// the same transcript in several homes, and those copies must keep one event
/// identity. `session_id` identifies the transcript that owns the line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeSource {
    pub source_key: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub is_subagent: bool,
}

impl ClaudeSource {
    pub fn new(source_key: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            source_key: source_key.into(),
            session_id: session_id.into(),
            parent_session_id: None,
            is_subagent: false,
        }
    }

    pub fn subagent(
        source_key: impl Into<String>,
        session_id: impl Into<String>,
        parent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            source_key: source_key.into(),
            session_id: session_id.into(),
            parent_session_id: Some(parent_session_id.into()),
            is_subagent: true,
        }
    }

    /// Derive main-session/subagent ownership from a path below
    /// `<config_root>/projects`.
    ///
    /// Both legacy flat files (`project/session.jsonl`) and modern nested files
    /// (`project/session/chat.jsonl`, `project/session/subagents/*.jsonl`, and
    /// workflow subagent descendants) are supported. The method is lexical so
    /// it also works for files that are being discovered but do not yet exist.
    pub fn from_path(path: impl AsRef<Path>, config_root: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let config_root = config_root.as_ref();
        let root_is_projects = config_root.file_name().and_then(|v| v.to_str()) == Some("projects");
        let projects_root = if root_is_projects {
            config_root.to_path_buf()
        } else {
            config_root.join("projects")
        };
        let relative = path.strip_prefix(&projects_root).unwrap_or(path);
        let parts = normal_components(relative);
        let project = parts.first().cloned().unwrap_or_else(|| "unknown".into());
        let source_key = short_hash(project.as_bytes());

        let file_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
            .to_owned();
        let subagents_index = parts.iter().position(|part| part == "subagents");

        if subagents_index.is_some() {
            let parent_session_id = parts.get(1).cloned().unwrap_or_else(|| "unknown".into());
            // `journal.jsonl` contains no assistant usage and therefore emits
            // no events, but treating it as a subagent source remains harmless.
            return Self::subagent(source_key, file_stem, parent_session_id);
        }

        let session_id = if parts.len() == 2 {
            // Legacy `project/session.jsonl`.
            file_stem
        } else {
            // Modern `project/session/chat.jsonl` (or another session-local
            // transcript filename).
            parts.get(1).cloned().unwrap_or(file_stem)
        };
        Self::new(source_key, session_id)
    }
}

/// Result of reconciling events collected from more than one Claude transcript.
#[derive(Clone, Debug, Default)]
pub struct ReconciledEvents {
    pub events: Vec<UsageEventDraft>,
    pub duplicate_records: u64,
}

/// Parse all complete JSONL records from one transcript snapshot. A non-empty
/// trailing fragment is deferred rather than classified as malformed; the next
/// full scan will retry it after the writer finishes the record.
pub fn parse_jsonl(bytes: &[u8], source: &ClaudeSource) -> ParseOutcome {
    let mut outcome = ParseOutcome::default();
    outcome.diagnostics.files_scanned = 1;
    outcome.diagnostics.last_scanned_at = Some(Utc::now().timestamp_millis());

    let mut candidates: HashMap<ExactDedupeKey, ParsedCandidate> = HashMap::new();
    let mut line_start = 0usize;

    for (newline_index, _) in bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n') {
        let mut line = &bytes[line_start..newline_index];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        parse_line(line, source, &mut candidates, &mut outcome.diagnostics);

        line_start = newline_index.saturating_add(1);
    }

    if bytes[line_start..]
        .iter()
        .any(|byte| !byte.is_ascii_whitespace())
    {
        outcome.diagnostics.is_partial = true;
    }

    let mut selected = candidates.into_values().collect::<Vec<_>>();
    selected.sort_by(|left, right| event_order(&left.event, &right.event));
    outcome.events = selected
        .into_iter()
        .filter(|candidate| has_billable_tokens(&candidate.event.tokens))
        .map(|candidate| candidate.event)
        .collect();
    update_coverage(&mut outcome.diagnostics, &outcome.events);
    outcome
}

/// Reconcile a full scan's events across main, subagent, and progress-log files.
///
/// Exact duplicates use `(source namespace, message.id, requestId)`. If a
/// sidechain copied a parent message under a *different* request ID, the
/// non-sidechain parent wins. Distinct sidechain answers have distinct message
/// IDs and remain billable. Callers may combine freshly parsed events with
/// drafts from every discovered transcript before invoking this pure function.
pub fn reconcile_events(events: Vec<UsageEventDraft>) -> ReconciledEvents {
    let mut result = ReconciledEvents::default();
    let mut exact: BTreeMap<ReconcileExactKey, TaggedEvent> = BTreeMap::new();

    for event in events {
        let tag = EventKeyTag::parse(&event.source_event_key);
        let exact_key = ReconcileExactKey {
            namespace: tag
                .as_ref()
                .map(|tag| tag.namespace.clone())
                .unwrap_or_else(|| event.source_event_key.clone()),
            message_id: event.message_id.clone(),
            request_id: event.request_id.clone(),
        };
        let tagged = TaggedEvent {
            event,
            is_sidechain: tag.is_some_and(|tag| tag.is_sidechain),
        };

        match exact.entry(exact_key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(tagged);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                result.duplicate_records = result.duplicate_records.saturating_add(1);
                if prefer_tagged_event(&tagged, entry.get()) == Ordering::Greater {
                    entry.insert(tagged);
                }
            }
        }
    }

    let mut by_message: BTreeMap<(String, Option<String>), Vec<TaggedEvent>> = BTreeMap::new();
    for (key, event) in exact {
        by_message
            .entry((key.namespace, key.message_id))
            .or_default()
            .push(event);
    }

    for (_, mut group) in by_message {
        if group.len() == 1 || !group.iter().any(|event| event.is_sidechain) {
            result
                .events
                .extend(group.into_iter().map(|item| item.event));
            continue;
        }

        let non_sidechain = group
            .iter()
            .enumerate()
            .filter_map(|(index, event)| (!event.is_sidechain).then_some(index))
            .collect::<Vec<_>>();
        if !non_sidechain.is_empty() {
            let removed = group.len().saturating_sub(non_sidechain.len());
            result.duplicate_records = result.duplicate_records.saturating_add(removed as u64);
            for index in non_sidechain {
                result.events.push(group[index].event.clone());
            }
            continue;
        }

        // Multiple sidechain copies with the same message ID are replays of the
        // same response. Keep the most complete snapshot.
        let best_index = (1..group.len()).fold(0usize, |best, index| {
            if prefer_event(&group[index].event, &group[best].event) == Ordering::Greater {
                index
            } else {
                best
            }
        });
        result.duplicate_records = result
            .duplicate_records
            .saturating_add(group.len().saturating_sub(1) as u64);
        result.events.push(group.swap_remove(best_index).event);
    }

    result.events.sort_by(event_order);
    result
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<RawTimestamp>,
    #[serde(alias = "session_id")]
    session_id: Option<String>,
    request_id: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    is_sidechain: Option<bool>,
    message: Option<RawMessage>,
    data: Option<RawProgressData>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawProgressData {
    message: Option<RawProgressMessage>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProgressMessage {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<RawTimestamp>,
    #[serde(alias = "session_id")]
    session_id: Option<String>,
    request_id: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    is_sidechain: Option<bool>,
    message: Option<RawMessage>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawMessage {
    role: Option<String>,
    id: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<RawCacheCreation>,
}

impl RawUsage {
    fn cache_write_tokens(self) -> u64 {
        self.cache_creation
            .map_or(self.cache_creation_input_tokens, |value| {
                value
                    .ephemeral_5m_input_tokens
                    .saturating_add(value.ephemeral_1h_input_tokens)
            })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct RawCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum RawTimestamp {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

#[derive(Clone, Debug)]
struct NormalizedRecord {
    timestamp: Option<RawTimestamp>,
    session_id: Option<String>,
    request_id: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    is_sidechain: bool,
    message: RawMessage,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExactDedupeKey {
    message_id: String,
    request_id: Option<String>,
}

#[derive(Clone, Debug)]
struct ParsedCandidate {
    event: UsageEventDraft,
    is_final: bool,
}

#[derive(Clone, Debug)]
struct TaggedEvent {
    event: UsageEventDraft,
    is_sidechain: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReconcileExactKey {
    namespace: String,
    message_id: Option<String>,
    request_id: Option<String>,
}

#[derive(Clone, Debug)]
struct EventKeyTag {
    namespace: String,
    is_sidechain: bool,
}

impl EventKeyTag {
    fn parse(key: &str) -> Option<Self> {
        let mut parts = key.split(':');
        (parts.next()? == EVENT_KEY_PREFIX).then_some(())?;
        (parts.next()?.parse::<u32>().ok()? == PARSER_VERSION).then_some(())?;
        let namespace = parts.next()?.to_owned();
        let kind = parts.next()?;
        // Consume the pair hash and reject ambiguous extra separators.
        (!parts.next()?.is_empty() && parts.next().is_none()).then_some(())?;
        let is_sidechain = match kind {
            "s" => true,
            "m" => false,
            _ => return None,
        };
        Some(Self {
            namespace,
            is_sidechain,
        })
    }
}

fn parse_line(
    line: &[u8],
    source: &ClaudeSource,
    candidates: &mut HashMap<ExactDedupeKey, ParsedCandidate>,
    diagnostics: &mut UsageDiagnostics,
) {
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }

    let raw = match serde_json::from_slice::<RawLine>(line) {
        Ok(raw) => raw,
        Err(_) => {
            diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
            return;
        }
    };
    let Some(record) = normalize_record(raw) else {
        return;
    };
    let Some(usage) = record.message.usage else {
        return;
    };
    let Some(message_id) = nonempty(record.message.id) else {
        diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
        return;
    };
    let request_id = nonempty(record.request_id);
    let Some(occurred_at_ms) = record.timestamp.as_ref().and_then(parse_timestamp_ms) else {
        diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
        return;
    };

    let session_id = if source.is_subagent {
        source.session_id.clone()
    } else {
        nonempty(record.session_id).unwrap_or_else(|| source.session_id.clone())
    };
    if session_id.is_empty() {
        diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
        return;
    }

    let is_final = record.message.stop_reason.is_some();
    let quality = if request_id.is_none() {
        UsageQuality::Heuristic
    } else if is_final {
        UsageQuality::Exact
    } else {
        UsageQuality::Normalized
    };
    let namespace = short_hash(source.source_key.as_bytes());
    let source_event_key = event_key(
        &namespace,
        &message_id,
        request_id.as_deref(),
        record.is_sidechain,
    );
    let model = nonempty(record.message.model).filter(|model| model != "<synthetic>");
    let event = UsageEventDraft {
        platform: Platform::ClaudeCode,
        source_event_key,
        session_id,
        parent_session_id: source.parent_session_id.clone(),
        turn_id: nonempty(record.parent_uuid).or_else(|| nonempty(record.uuid)),
        request_id: request_id.clone(),
        message_id: Some(message_id.clone()),
        occurred_at_ms,
        model,
        tokens: TokenBreakdown {
            uncached_input: usage.input_tokens,
            cache_read: usage.cache_read_input_tokens,
            cache_write: usage.cache_write_tokens(),
            output: usage.output_tokens,
            reasoning_output: 0,
        },
        request_count: 1,
        quality,
    };
    let key = ExactDedupeKey {
        message_id,
        request_id,
    };
    let candidate = ParsedCandidate { event, is_final };

    match candidates.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(candidate);
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            diagnostics.duplicate_records = diagnostics.duplicate_records.saturating_add(1);
            if prefer_candidate(&candidate, entry.get()) == Ordering::Greater {
                entry.insert(candidate);
            }
        }
    }
}

fn normalize_record(raw: RawLine) -> Option<NormalizedRecord> {
    if let Some(message) = raw.message
        && is_assistant(raw.kind.as_deref(), message.role.as_deref())
    {
        return Some(NormalizedRecord {
            timestamp: raw.timestamp,
            session_id: raw.session_id,
            request_id: raw.request_id,
            uuid: raw.uuid,
            parent_uuid: raw.parent_uuid,
            is_sidechain: raw.is_sidechain.unwrap_or(false),
            message,
        });
    }

    let nested = raw.data?.message?;
    let message = nested.message?;
    is_assistant(nested.kind.as_deref(), message.role.as_deref()).then_some(NormalizedRecord {
        timestamp: nested.timestamp.or(raw.timestamp),
        session_id: nested.session_id.or(raw.session_id),
        request_id: nested.request_id.or(raw.request_id),
        uuid: nested.uuid.or(raw.uuid),
        parent_uuid: nested.parent_uuid.or(raw.parent_uuid),
        is_sidechain: nested.is_sidechain.or(raw.is_sidechain).unwrap_or(false),
        message,
    })
}

fn is_assistant(kind: Option<&str>, role: Option<&str>) -> bool {
    if kind.is_some_and(|value| value != "assistant") {
        return false;
    }
    if role.is_some_and(|value| value != "assistant") {
        return false;
    }
    kind == Some("assistant") || role == Some("assistant") || (kind.is_none() && role.is_none())
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn parse_timestamp_ms(timestamp: &RawTimestamp) -> Option<i64> {
    match timestamp {
        RawTimestamp::Text(value) => DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.timestamp_millis()),
        RawTimestamp::Signed(value) => normalize_unix_timestamp(*value),
        RawTimestamp::Unsigned(value) => i64::try_from(*value)
            .ok()
            .and_then(normalize_unix_timestamp),
        RawTimestamp::Float(value) if value.is_finite() && *value >= 0.0 => {
            let millis = if *value >= 100_000_000_000.0 {
                *value
            } else {
                *value * 1_000.0
            };
            (millis <= i64::MAX as f64).then_some(millis.round() as i64)
        }
        RawTimestamp::Float(_) => None,
    }
}

fn normalize_unix_timestamp(value: i64) -> Option<i64> {
    if value < 0 {
        return None;
    }
    if value >= 100_000_000_000 {
        Some(value)
    } else {
        value.checked_mul(1_000)
    }
}

fn prefer_candidate(candidate: &ParsedCandidate, existing: &ParsedCandidate) -> Ordering {
    candidate
        .is_final
        .cmp(&existing.is_final)
        .then_with(|| prefer_event(&candidate.event, &existing.event))
}

fn prefer_event(candidate: &UsageEventDraft, existing: &UsageEventDraft) -> Ordering {
    quality_rank(candidate.quality)
        .cmp(&quality_rank(existing.quality))
        .then_with(|| {
            candidate
                .parent_session_id
                .is_some()
                .cmp(&existing.parent_session_id.is_some())
        })
        .then_with(|| token_total(&candidate.tokens).cmp(&token_total(&existing.tokens)))
        .then_with(|| candidate.tokens.output.cmp(&existing.tokens.output))
        .then_with(|| candidate.occurred_at_ms.cmp(&existing.occurred_at_ms))
}

fn prefer_tagged_event(candidate: &TaggedEvent, existing: &TaggedEvent) -> Ordering {
    // A sidechain copy can contain a much larger cache-read snapshot than the
    // parent it replays. Completeness must never override canonical provenance.
    existing
        .is_sidechain
        .cmp(&candidate.is_sidechain)
        .then_with(|| prefer_event(&candidate.event, &existing.event))
}

const fn quality_rank(quality: UsageQuality) -> u8 {
    match quality {
        UsageQuality::Heuristic => 0,
        UsageQuality::Normalized => 1,
        UsageQuality::Exact => 2,
    }
}

fn has_billable_tokens(tokens: &TokenBreakdown) -> bool {
    token_total(tokens) > 0
}

fn token_total(tokens: &TokenBreakdown) -> u64 {
    tokens
        .uncached_input
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.output)
}

fn event_order(left: &UsageEventDraft, right: &UsageEventDraft) -> Ordering {
    left.occurred_at_ms
        .cmp(&right.occurred_at_ms)
        .then_with(|| left.source_event_key.cmp(&right.source_event_key))
}

fn update_coverage(diagnostics: &mut UsageDiagnostics, events: &[UsageEventDraft]) {
    diagnostics.coverage_start = events.iter().map(|event| event.occurred_at_ms).min();
    diagnostics.coverage_end = events.iter().map(|event| event.occurred_at_ms).max();
}

fn event_key(
    namespace: &str,
    message_id: &str,
    request_id: Option<&str>,
    is_sidechain: bool,
) -> String {
    let mut material =
        Vec::with_capacity(message_id.len() + request_id.map_or(0, str::len).saturating_add(24));
    append_hash_component(&mut material, message_id.as_bytes());
    match request_id {
        Some(request_id) => {
            material.push(1);
            append_hash_component(&mut material, request_id.as_bytes());
        }
        None => material.push(0),
    }
    let pair_hash = short_hash(&material);
    let kind = if is_sidechain { "s" } else { "m" };
    format!("{EVENT_KEY_PREFIX}:{PARSER_VERSION}:{namespace}:{kind}:{pair_hash}")
}

fn short_hash(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    hex::encode(&digest[..16])
}

fn append_hash_component(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn source() -> ClaudeSource {
        ClaudeSource::new("home-a/project-a", "session-a")
    }

    fn parse(input: &str) -> ParseOutcome {
        parse_jsonl(input.as_bytes(), &source())
    }

    #[test]
    fn parses_usage_metadata_without_needing_message_content() {
        let input = r#"{"type":"assistant","sessionId":"session-a","timestamp":"2026-04-05T12:00:00Z","requestId":"req-1","uuid":"turn-1","message":{"role":"assistant","id":"msg-1","model":"claude-opus-4-6","content":{"an":"arbitrary shape that is intentionally ignored"},"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":150,"cache_read_input_tokens":5000,"cache_creation_input_tokens":10000}}}
"#;
        let outcome = parse(input);

        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        assert_eq!(event.platform, Platform::ClaudeCode);
        assert_eq!(event.session_id, "session-a");
        assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(event.request_id.as_deref(), Some("req-1"));
        assert_eq!(event.message_id.as_deref(), Some("msg-1"));
        assert_eq!(event.occurred_at_ms, 1_775_390_400_000);
        assert_eq!(event.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(event.tokens.uncached_input, 3);
        assert_eq!(event.tokens.cache_read, 5000);
        assert_eq!(event.tokens.cache_write, 10000);
        assert_eq!(event.tokens.output, 150);
        assert_eq!(event.quality, UsageQuality::Exact);
        assert_eq!(outcome.diagnostics.malformed_records, 0);
    }

    #[test]
    fn duration_breakdown_replaces_legacy_cache_creation_total() {
        let input = concat!(
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:00Z","requestId":"req-a","message":{"id":"msg-a","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":999,"cache_creation":{"ephemeral_5m_input_tokens":7,"ephemeral_1h_input_tokens":11}}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:01Z","requestId":"req-b","message":{"id":"msg-b","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":23}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:02Z","requestId":"req-c","message":{"id":"msg-c","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":999,"cache_creation":{}}}}"#,
            "\n",
        );
        let outcome = parse(input);

        assert_eq!(outcome.events.len(), 3);
        assert_eq!(outcome.events[0].tokens.cache_write, 18);
        assert_eq!(outcome.events[1].tokens.cache_write, 23);
        assert_eq!(outcome.events[2].tokens.cache_write, 0);
    }

    #[test]
    fn exact_pair_dedup_keeps_final_snapshot_but_distinct_requests() {
        let input = concat!(
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:00Z","requestId":"req-1","message":{"id":"msg-1","usage":{"input_tokens":3,"output_tokens":1}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:01Z","requestId":"req-1","message":{"id":"msg-1","stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":100}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:02Z","requestId":"req-2","message":{"id":"msg-1","stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":200}}}"#,
            "\n",
        );
        let outcome = parse(input);

        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.diagnostics.duplicate_records, 1);
        assert_eq!(outcome.events[0].request_id.as_deref(), Some("req-1"));
        assert_eq!(outcome.events[0].tokens.output, 100);
        assert_eq!(outcome.events[0].quality, UsageQuality::Exact);
        assert_eq!(outcome.events[1].request_id.as_deref(), Some("req-2"));
        assert_ne!(
            outcome.events[0].source_event_key,
            outcome.events[1].source_event_key
        );
    }

    #[test]
    fn preliminary_and_final_batches_reuse_the_same_upsert_key() {
        let preliminary = parse(concat!(
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:00Z","requestId":"req-1","message":{"id":"msg-1","usage":{"input_tokens":3,"output_tokens":1}}}"#,
            "\n"
        ));
        let final_snapshot = parse(concat!(
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:01Z","requestId":"req-1","message":{"id":"msg-1","stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":100}}}"#,
            "\n"
        ));

        assert_eq!(preliminary.events.len(), 1);
        assert_eq!(final_snapshot.events.len(), 1);
        assert_eq!(
            preliminary.events[0].source_event_key,
            final_snapshot.events[0].source_event_key
        );
        assert_eq!(preliminary.events[0].quality, UsageQuality::Normalized);
        assert_eq!(final_snapshot.events[0].quality, UsageQuality::Exact);
    }

    #[test]
    fn reconciles_sidechain_parent_replays_without_dropping_real_answers() {
        let parent = parse(concat!(
            r#"{"type":"assistant","timestamp":"2026-03-29T07:00:00Z","requestId":"req-parent","message":{"id":"msg-parent","stop_reason":"end_turn","usage":{"output_tokens":10,"cache_read_input_tokens":20}}}"#,
            "\n"
        ));
        let sidechain = parse(concat!(
            r#"{"type":"assistant","timestamp":"2026-03-29T07:00:01Z","requestId":"req-replay","isSidechain":true,"message":{"id":"msg-parent","stop_reason":"end_turn","usage":{"output_tokens":10,"cache_read_input_tokens":50000}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-03-29T07:00:02Z","requestId":"req-answer","isSidechain":true,"message":{"id":"msg-answer","stop_reason":"end_turn","usage":{"output_tokens":30,"cache_read_input_tokens":700}}}"#,
            "\n",
        ));
        let reconciled =
            reconcile_events(parent.events.into_iter().chain(sidechain.events).collect());

        assert_eq!(reconciled.events.len(), 2);
        assert_eq!(reconciled.duplicate_records, 1);
        let kept_parent = reconciled
            .events
            .iter()
            .find(|event| event.message_id.as_deref() == Some("msg-parent"))
            .unwrap();
        assert_eq!(kept_parent.request_id.as_deref(), Some("req-parent"));
        assert_eq!(kept_parent.tokens.cache_read, 20);
        assert!(
            reconciled
                .events
                .iter()
                .any(|event| event.message_id.as_deref() == Some("msg-answer"))
        );
    }

    #[test]
    fn parses_progress_replay_and_prefers_direct_subagent_source() {
        let progress_source = ClaudeSource::new("same-home-project", "parent-session");
        let direct_source =
            ClaudeSource::subagent("same-home-project", "agent-a", "parent-session");
        let progress_line = concat!(
            r#"{"type":"progress","data":{"message":{"type":"assistant","timestamp":"2026-03-10T06:00:01Z","requestId":"req-agent","isSidechain":true,"message":{"role":"assistant","id":"msg-agent","model":"claude-haiku-4-5","stop_reason":"end_turn","content":[{"type":"text","text":"ignored"}],"usage":{"input_tokens":20,"output_tokens":2}}}}}"#,
            "\n"
        );
        let direct_line = concat!(
            r#"{"type":"assistant","timestamp":"2026-03-10T06:00:01Z","sessionId":"parent-session","requestId":"req-agent","message":{"role":"assistant","id":"msg-agent","model":"claude-haiku-4-5","stop_reason":"end_turn","usage":{"input_tokens":20,"output_tokens":2}}}"#,
            "\n"
        );
        let progress = parse_jsonl(progress_line.as_bytes(), &progress_source);
        let direct = parse_jsonl(direct_line.as_bytes(), &direct_source);
        let reconciled =
            reconcile_events(progress.events.into_iter().chain(direct.events).collect());

        assert_eq!(reconciled.events.len(), 1);
        assert_eq!(reconciled.duplicate_records, 1);
        assert_eq!(reconciled.events[0].session_id, "agent-a");
        assert_eq!(
            reconciled.events[0].parent_session_id.as_deref(),
            Some("parent-session")
        );
    }

    #[test]
    fn defers_non_newline_tail_and_commits_complete_malformed_lines() {
        let complete = "{not-json}\n";
        let partial = r#"{"type":"assistant","timestamp":"2026-04"#;
        let input = format!("{complete}{partial}");
        let outcome = parse_jsonl(input.as_bytes(), &source());

        assert_eq!(outcome.diagnostics.malformed_records, 1);
        assert!(outcome.diagnostics.is_partial);
    }

    #[test]
    fn skips_zero_usage_and_marks_missing_request_as_heuristic() {
        let input = concat!(
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:00Z","message":{"id":"msg-zero","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-04-05T12:00:01Z","message":{"id":"msg-no-request","usage":{"input_tokens":1,"output_tokens":2}}}"#,
            "\n",
        );
        let outcome = parse(input);

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].message_id.as_deref(),
            Some("msg-no-request")
        );
        assert_eq!(outcome.events[0].quality, UsageQuality::Heuristic);
    }

    #[test]
    fn derives_main_and_subagent_sources_from_paths() {
        let main = ClaudeSource::from_path(
            "/home/me/.claude/projects/project-a/session-a.jsonl",
            "/home/me/.claude",
        );
        let nested = ClaudeSource::from_path(
            "/home/me/.claude/projects/project-a/session-b/chat.jsonl",
            "/home/me/.claude",
        );
        let agent = ClaudeSource::from_path(
            "/home/me/.claude/projects/project-a/session-b/subagents/agent-1.jsonl",
            "/home/me/.claude",
        );
        let workflow_agent = ClaudeSource::from_path(
            "/home/me/.claude/projects/project-a/session-b/subagents/workflows/wf-1/agent-2.jsonl",
            "/home/me/.claude",
        );

        assert_eq!(main.session_id, "session-a");
        assert!(!main.is_subagent);
        assert_eq!(nested.session_id, "session-b");
        assert!(!nested.is_subagent);
        assert_eq!(agent.session_id, "agent-1");
        assert!(agent.is_subagent);
        assert_eq!(agent.parent_session_id.as_deref(), Some("session-b"));
        assert_eq!(workflow_agent.session_id, "agent-2");
        assert_eq!(
            workflow_agent.parent_session_id.as_deref(),
            Some("session-b")
        );
        assert_eq!(main.source_key, agent.source_key);

        let other_home = ClaudeSource::from_path(
            "/other/.claude/projects/project-a/session-a.jsonl",
            "/other/.claude",
        );
        assert_eq!(main.source_key, other_home.source_key);

        let projects_root = ClaudeSource::from_path(
            "/home/me/.claude/projects/project-a/session-a.jsonl",
            "/home/me/.claude/projects",
        );
        assert_eq!(main.source_key, projects_root.source_key);
    }
}
