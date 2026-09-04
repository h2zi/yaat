//! Metadata-only parser for Codex session usage snapshots.

#[cfg(test)]
use std::io::Cursor;
use std::io::{self, BufRead};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use yaat_contracts::{Platform, TokenBreakdown, UsageDiagnostics};

use super::{ParseOutcome, UsageEventDraft, UsageQuality};

const CHECKPOINT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CodexCheckpoint {
    version: u32,
    state: ParserState,
}

impl CodexCheckpoint {
    pub fn is_current(&self) -> bool {
        self.version == CHECKPOINT_VERSION
    }
}

pub struct StreamParseOutcome {
    pub outcome: ParseOutcome,
    pub checkpoint: CodexCheckpoint,
    pub consumed_bytes: u64,
}

/// Parse one complete Codex transcript snapshot. An incomplete final record is
/// ignored and retried on the next full scan.
#[cfg(test)]
pub fn parse_bytes(bytes: &[u8]) -> ParseOutcome {
    parse_reader(&mut Cursor::new(bytes), "", 0, None)
        .expect("in-memory Codex transcript reads cannot fail")
        .outcome
}

pub fn parse_reader<R: BufRead>(
    reader: &mut R,
    rollout_id: &str,
    start_offset: u64,
    checkpoint: Option<CodexCheckpoint>,
) -> io::Result<StreamParseOutcome> {
    let mut state = checkpoint
        .filter(|checkpoint| checkpoint.version == CHECKPOINT_VERSION)
        .map_or_else(ParserState::default, |checkpoint| checkpoint.state);
    let mut diagnostics = UsageDiagnostics {
        files_scanned: 1,
        last_scanned_at: Some(now_ms()),
        ..UsageDiagnostics::default()
    };
    let mut events = Vec::new();
    let mut consumed_bytes = 0_u64;
    let mut line = Vec::new();

    loop {
        line.clear();
        let consumed = reader.read_until(b'\n', &mut line)?;
        if consumed == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            if line.iter().any(|byte| !byte.is_ascii_whitespace()) {
                diagnostics.is_partial = true;
            }
            break;
        }
        let record_start = start_offset.saturating_add(consumed_bytes);
        consumed_bytes = consumed_bytes.saturating_add(consumed as u64);
        line.pop();
        let record = line.strip_suffix(b"\r").unwrap_or(&line);

        if record.iter().all(u8::is_ascii_whitespace) {
            continue;
        }

        match parse_record(
            record,
            record_start,
            rollout_id,
            &mut state,
            &mut diagnostics,
        ) {
            Ok(event) => {
                if let Some(event) = event {
                    update_coverage(&mut diagnostics, event.occurred_at_ms);
                    events.push(event);
                }
            }
            Err(()) => {
                diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
                diagnostics.is_partial = true;
            }
        }
    }

    Ok(StreamParseOutcome {
        outcome: ParseOutcome {
            events,
            diagnostics,
        },
        checkpoint: CodexCheckpoint {
            version: CHECKPOINT_VERSION,
            state,
        },
        consumed_bytes,
    })
}

fn parse_record(
    record: &[u8],
    record_offset: u64,
    rollout_id: &str,
    state: &mut ParserState,
    diagnostics: &mut UsageDiagnostics,
) -> Result<Option<UsageEventDraft>, ()> {
    let header: RecordHeader = serde_json::from_slice(record).map_err(|_| ())?;
    match header.kind.as_str() {
        "session_meta" => {
            let record: SessionMetaRecord = serde_json::from_slice(record).map_err(|_| ())?;
            state.apply_session_meta(record.payload);
            Ok(None)
        }
        "turn_context" => {
            let record: TurnContextRecord = serde_json::from_slice(record).map_err(|_| ())?;
            state.apply_turn_context(record.payload);
            Ok(None)
        }
        "event_msg" => {
            let event_header: EventHeader = serde_json::from_slice(record).map_err(|_| ())?;
            if event_header.payload.kind != "token_count" {
                return Ok(None);
            }
            let record: TokenCountRecord = serde_json::from_slice(record).map_err(|_| ())?;
            state.apply_token_count(record, record_offset, rollout_id, diagnostics)
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
struct RecordHeader {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct EventHeader {
    payload: EventHeaderPayload,
}

#[derive(Debug, Deserialize)]
struct EventHeaderPayload {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct SessionMetaRecord {
    payload: SessionMetaPayload,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: Option<String>,
    session_id: Option<String>,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnContextRecord {
    payload: TurnContextPayload,
}

#[derive(Debug, Deserialize)]
struct TurnContextPayload {
    turn_id: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenCountRecord {
    timestamp: JsonTimestamp,
    payload: TokenCountPayload,
}

#[derive(Debug, Deserialize)]
struct TokenCountPayload {
    info: Option<TokenCountInfo>,
}

#[derive(Debug, Deserialize)]
struct TokenCountInfo {
    total_token_usage: Option<RawTokenUsage>,
    last_token_usage: Option<RawTokenUsage>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
struct RawTokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl RawTokenUsage {
    fn is_zero(self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_write_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
            && self.total_tokens == 0
    }

    fn dominates(self, other: Self) -> bool {
        self.input_tokens >= other.input_tokens
            && self.cached_input_tokens >= other.cached_input_tokens
            && self.cache_write_input_tokens >= other.cache_write_input_tokens
            && self.output_tokens >= other.output_tokens
            && self.reasoning_output_tokens >= other.reasoning_output_tokens
            && self.effective_total() >= other.effective_total()
    }

    fn delta_from(self, previous: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_sub(previous.cache_write_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(previous.reasoning_output_tokens),
            total_tokens: self
                .effective_total()
                .saturating_sub(previous.effective_total()),
        }
    }

    fn effective_total(self) -> u64 {
        if self.total_tokens == 0 {
            self.input_tokens.saturating_add(self.output_tokens)
        } else {
            self.total_tokens
        }
    }

    fn normalize(self) -> (TokenBreakdown, bool) {
        let cache_read = self.cached_input_tokens.min(self.input_tokens);
        let cache_write = self
            .cache_write_input_tokens
            .min(self.input_tokens.saturating_sub(cache_read));
        let uncached_input = self
            .input_tokens
            .saturating_sub(cache_read)
            .saturating_sub(cache_write);
        let reasoning_output = self.reasoning_output_tokens.min(self.output_tokens);
        let was_clamped = cache_read != self.cached_input_tokens
            || cache_write != self.cache_write_input_tokens
            || reasoning_output != self.reasoning_output_tokens
            || (self.total_tokens != 0
                && self.total_tokens != self.input_tokens.saturating_add(self.output_tokens));

        (
            TokenBreakdown {
                uncached_input,
                cache_read,
                cache_write,
                output: self.output_tokens,
                reasoning_output,
            },
            was_clamped,
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonTimestamp {
    Text(String),
    Integer(i64),
    Float(f64),
}

impl JsonTimestamp {
    fn into_millis(self) -> Option<i64> {
        match self {
            Self::Text(value) => DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|timestamp| timestamp.timestamp_millis()),
            Self::Integer(value) => Some(normalize_epoch(value)),
            Self::Float(value) if value.is_finite() => {
                let value = value.round();
                if value < i64::MIN as f64 || value > i64::MAX as f64 {
                    None
                } else {
                    Some(normalize_epoch(value as i64))
                }
            }
            Self::Float(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ParserState {
    session_id: Option<String>,
    parent_session_id: Option<String>,
    session_meta_count: u64,
    replay_turns_remaining: Option<u64>,
    current_started: bool,
    turn_id: Option<String>,
    model: Option<String>,
    cumulative_high_water: Option<RawTokenUsage>,
}

impl ParserState {
    fn apply_session_meta(&mut self, payload: SessionMetaPayload) {
        let record_session_id = payload.id.or(payload.session_id);
        if self.session_meta_count == 0 {
            self.session_id = record_session_id;
            self.parent_session_id = payload
                .parent_thread_id
                .clone()
                .or_else(|| payload.forked_from_id.clone());
            self.current_started = self.parent_session_id.is_none();
        }

        self.session_meta_count = self.session_meta_count.saturating_add(1);
    }

    fn apply_turn_context(&mut self, payload: TurnContextPayload) {
        let remaining = self
            .replay_turns_remaining
            .get_or_insert_with(|| self.session_meta_count.saturating_sub(1));
        if *remaining > 0 {
            *remaining -= 1;
            self.current_started = false;
            self.turn_id = None;
            self.model = None;
            return;
        }

        self.current_started = true;
        self.turn_id = payload.turn_id;
        self.model = payload.model;
    }

    fn apply_token_count(
        &mut self,
        record: TokenCountRecord,
        record_offset: u64,
        rollout_id: &str,
        diagnostics: &mut UsageDiagnostics,
    ) -> Result<Option<UsageEventDraft>, ()> {
        let occurred_at_ms = record.timestamp.into_millis().ok_or(())?;
        let Some(info) = record.payload.info else {
            return Ok(None);
        };
        let Some(total) = info.total_token_usage else {
            return Ok(None);
        };

        let previous = self.cumulative_high_water;
        if previous == Some(total) {
            diagnostics.duplicate_records = diagnostics.duplicate_records.saturating_add(1);
            return Ok(None);
        }

        let (delta, mut quality, replace_high_water) = match previous {
            None => (total, UsageQuality::Exact, true),
            Some(previous) if total.dominates(previous) => {
                (total.delta_from(previous), UsageQuality::Exact, true)
            }
            Some(_) => {
                // A lower snapshot may be a genuine counter reset or an interleaved stale stream.
                // `last_token_usage` is non-cumulative and is the only lossless value in either
                // case. A reset normally has last == total; otherwise retain the high-water mark
                // so returning to the original stream still produces the right cumulative delta.
                let fallback = info.last_token_usage.unwrap_or(total);
                let is_reset = fallback == total || total.effective_total() == 0;
                (fallback, UsageQuality::Heuristic, is_reset)
            }
        };

        if replace_high_water {
            self.cumulative_high_water = Some(total);
        }

        if !self.current_started || delta.is_zero() {
            if delta.is_zero() {
                diagnostics.duplicate_records = diagnostics.duplicate_records.saturating_add(1);
            }
            return Ok(None);
        }

        let session_id = self.session_id.clone().ok_or(())?;
        let (tokens, was_clamped) = delta.normalize();
        if was_clamped {
            quality = UsageQuality::Heuristic;
            diagnostics.is_partial = true;
        }

        Ok(Some(UsageEventDraft {
            platform: Platform::Codex,
            source_event_key: format!(
                "codex:{}:{record_offset}",
                if rollout_id.is_empty() {
                    session_id.as_str()
                } else {
                    rollout_id
                }
            ),
            session_id,
            parent_session_id: self.parent_session_id.clone(),
            turn_id: self.turn_id.clone(),
            request_id: None,
            message_id: None,
            occurred_at_ms,
            model: self.model.clone(),
            tokens,
            request_count: 1,
            quality,
        }))
    }
}

fn update_coverage(diagnostics: &mut UsageDiagnostics, occurred_at_ms: i64) {
    diagnostics.coverage_start = Some(
        diagnostics
            .coverage_start
            .map_or(occurred_at_ms, |current| current.min(occurred_at_ms)),
    );
    diagnostics.coverage_end = Some(
        diagnostics
            .coverage_end
            .map_or(occurred_at_ms, |current| current.max(occurred_at_ms)),
    );
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn normalize_epoch(value: i64) -> i64 {
    // Values below 10^11 cannot be contemporary millisecond timestamps and are treated as
    // seconds. This also accepts historical fixtures without coupling the parser to a date range.
    if value.unsigned_abs() < 100_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn meta(id: &str, parent: Option<&str>) -> String {
        let parent = parent
            .map(|value| format!(r#","parent_thread_id":"{value}""#))
            .unwrap_or_default();
        format!(
            r#"{{"type":"session_meta","timestamp":"2026-08-01T00:00:00Z","payload":{{"id":"{id}"{parent},"base_instructions":"private body"}}}}"#
        )
    }

    fn turn(id: &str, model: &str) -> String {
        format!(
            r#"{{"type":"turn_context","timestamp":"2026-08-01T00:00:01Z","payload":{{"turn_id":"{id}","model":"{model}","summary":"private body"}}}}"#
        )
    }

    fn usage(timestamp: &str, total: [u64; 6], last: Option<[u64; 6]>) -> String {
        let raw = |values: [u64; 6]| {
            format!(
                r#"{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{}}}"#,
                values[0], values[1], values[2], values[3], values[4], values[5]
            )
        };
        let last = last
            .map(|values| format!(r#","last_token_usage":{}"#, raw(values)))
            .unwrap_or_default();
        format!(
            r#"{{"type":"event_msg","timestamp":"{timestamp}","payload":{{"type":"token_count","info":{{"total_token_usage":{}{last}}}}}}}"#,
            raw(total)
        )
    }

    fn jsonl(records: &[String]) -> Vec<u8> {
        let mut value = records.join("\n").into_bytes();
        value.push(b'\n');
        value
    }

    #[test]
    fn parses_cumulative_usage_and_token_subsets() {
        let bytes = jsonl(&[
            meta("session-a", None),
            turn("turn-a", "gpt-test"),
            usage(
                "2026-08-01T01:02:03.004Z",
                [100, 20, 10, 30, 5, 130],
                Some([100, 20, 10, 30, 5, 130]),
            ),
            usage(
                "2026-08-01T01:03:03.004Z",
                [160, 50, 15, 50, 8, 210],
                Some([60, 30, 5, 20, 3, 80]),
            ),
        ]);

        let result = parse_bytes(&bytes);
        assert_eq!(result.events.len(), 2);
        let first = &result.events[0];
        assert_eq!(first.session_id, "session-a");
        assert_eq!(first.turn_id.as_deref(), Some("turn-a"));
        assert_eq!(first.model.as_deref(), Some("gpt-test"));
        assert_eq!(first.tokens.uncached_input, 70);
        assert_eq!(first.tokens.cache_read, 20);
        assert_eq!(first.tokens.cache_write, 10);
        assert_eq!(first.tokens.output, 30);
        assert_eq!(first.tokens.reasoning_output, 5);
        assert_eq!(first.tokens.total(), 130);

        let second = &result.events[1];
        assert_eq!(second.tokens.uncached_input, 25);
        assert_eq!(second.tokens.cache_read, 30);
        assert_eq!(second.tokens.cache_write, 5);
        assert_eq!(second.tokens.output, 20);
        assert_eq!(second.tokens.reasoning_output, 3);
        assert_eq!(second.tokens.total(), 80);
        assert_eq!(second.quality, UsageQuality::Exact);
    }

    #[test]
    fn skips_duplicates_and_handles_reset_and_interleaved_snapshot() {
        let bytes = jsonl(&[
            meta("session-a", None),
            turn("turn-a", "gpt-test"),
            usage("2026-08-01T00:00:02Z", [100, 0, 0, 10, 2, 110], None),
            usage("2026-08-01T00:00:03Z", [100, 0, 0, 10, 2, 110], None),
            // A lower stream snapshot: count its last request but keep A's high-water mark.
            usage(
                "2026-08-01T00:00:04Z",
                [40, 0, 0, 4, 1, 44],
                Some([10, 0, 0, 1, 1, 11]),
            ),
            usage("2026-08-01T00:00:05Z", [150, 0, 0, 15, 3, 165], None),
            // last == total identifies a genuine reset and installs a new baseline.
            usage(
                "2026-08-01T00:00:06Z",
                [20, 0, 0, 2, 1, 22],
                Some([20, 0, 0, 2, 1, 22]),
            ),
            usage("2026-08-01T00:00:07Z", [35, 0, 0, 5, 2, 40], None),
        ]);

        let result = parse_bytes(&bytes);
        assert_eq!(result.events.len(), 5);
        assert_eq!(result.diagnostics.duplicate_records, 1);
        assert_eq!(result.events[0].tokens.total(), 110);
        assert_eq!(result.events[1].tokens.total(), 11);
        assert_eq!(result.events[1].quality, UsageQuality::Heuristic);
        assert_eq!(result.events[2].tokens.total(), 55);
        assert_eq!(result.events[3].tokens.total(), 22);
        assert_eq!(result.events[4].tokens.total(), 18);
    }

    #[test]
    fn clamps_invalid_subset_counters_without_double_counting_reasoning() {
        let bytes = jsonl(&[
            meta("session-a", None),
            turn("turn-a", "gpt-test"),
            usage("2026-08-01T00:00:02Z", [10, 8, 8, 4, 9, 14], None),
        ]);

        let result = parse_bytes(&bytes);
        let event = &result.events[0];
        assert_eq!(event.tokens.uncached_input, 0);
        assert_eq!(event.tokens.cache_read, 8);
        assert_eq!(event.tokens.cache_write, 2);
        assert_eq!(event.tokens.output, 4);
        assert_eq!(event.tokens.reasoning_output, 4);
        assert_eq!(event.tokens.total(), 14);
        assert_eq!(event.quality, UsageQuality::Heuristic);
        assert!(result.diagnostics.is_partial);
    }

    #[test]
    fn excludes_fork_parent_prefix_but_uses_its_cumulative_baseline() {
        let bytes = jsonl(&[
            meta("child", Some("parent")),
            meta("parent", None),
            usage("2026-08-01T00:00:01Z", [100, 20, 0, 10, 2, 110], None),
            turn("parent-turn", "gpt-parent"),
            usage("2026-08-01T00:00:02Z", [150, 30, 0, 15, 3, 165], None),
            turn("child-turn", "gpt-child"),
            usage("2026-08-01T00:00:03Z", [180, 40, 0, 25, 7, 205], None),
        ]);

        let result = parse_bytes(&bytes);
        assert_eq!(result.events.len(), 1);
        let event = &result.events[0];
        assert_eq!(event.session_id, "child");
        assert_eq!(event.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(event.turn_id.as_deref(), Some("child-turn"));
        assert_eq!(event.model.as_deref(), Some("gpt-child"));
        assert_eq!(event.tokens.uncached_input, 20);
        assert_eq!(event.tokens.cache_read, 10);
        assert_eq!(event.tokens.output, 10);
        assert_eq!(event.tokens.reasoning_output, 4);
    }

    #[test]
    fn retries_an_incomplete_tail_on_the_next_full_scan() {
        let prefix = jsonl(&[meta("session-a", None), turn("turn-a", "gpt-test")]);
        let record = usage("2026-08-01T00:00:02Z", [10, 0, 0, 2, 1, 12], None);
        let mut partial = prefix.clone();
        partial.extend_from_slice(&record.as_bytes()[..record.len() / 2]);

        let first = parse_reader(&mut Cursor::new(&partial), "rollout-a", 0, None).unwrap();
        assert_eq!(first.consumed_bytes, prefix.len() as u64);
        assert!(first.outcome.events.is_empty());
        assert_eq!(first.outcome.diagnostics.malformed_records, 0);
        assert!(first.outcome.diagnostics.is_partial);

        let mut complete = prefix;
        complete.extend_from_slice(record.as_bytes());
        complete.push(b'\n');
        let second = parse_bytes(&complete);
        assert_eq!(second.events.len(), 1);
        assert!(!second.diagnostics.is_partial);
    }

    #[test]
    fn event_keys_survive_active_to_archive_rescan() {
        let bytes = jsonl(&[
            meta("session-a", None),
            turn("turn-a", "gpt-test"),
            usage("2026-08-01T00:00:02Z", [10, 0, 0, 2, 1, 12], None),
        ]);

        let active = parse_reader(&mut Cursor::new(&bytes), "rollout-a.jsonl", 0, None)
            .unwrap()
            .outcome;
        let archived = parse_reader(&mut Cursor::new(&bytes), "rollout-a.jsonl", 0, None)
            .unwrap()
            .outcome;
        assert_eq!(
            active.events[0].source_event_key,
            archived.events[0].source_event_key
        );
        assert!(!active.events[0].source_event_key.contains("sessions"));
    }

    #[test]
    fn physical_rollout_id_scopes_events_for_one_logical_thread() {
        let bytes = jsonl(&[
            meta("thread-a", None),
            turn("turn-a", "gpt-test"),
            usage("2026-08-01T00:00:02Z", [10, 0, 0, 2, 1, 12], None),
        ]);
        let first = parse_reader(&mut Cursor::new(&bytes), "rollout-a", 0, None).unwrap();
        let second = parse_reader(&mut Cursor::new(&bytes), "rollout-b", 0, None).unwrap();

        assert_eq!(first.outcome.events[0].session_id, "thread-a");
        assert_eq!(second.outcome.events[0].session_id, "thread-a");
        assert_ne!(
            first.outcome.events[0].source_event_key,
            second.outcome.events[0].source_event_key
        );
    }

    #[test]
    fn checkpoint_continues_cumulative_usage_from_the_committed_byte() {
        let first_chunk = jsonl(&[
            meta("thread-a", None),
            turn("turn-a", "gpt-test"),
            usage("2026-08-01T00:00:02Z", [10, 0, 0, 2, 1, 12], None),
        ]);
        let second_chunk = jsonl(&[usage(
            "2026-08-01T00:00:03Z",
            [25, 5, 0, 5, 2, 30],
            Some([15, 5, 0, 3, 1, 18]),
        )]);
        let first = parse_reader(&mut Cursor::new(&first_chunk), "rollout-a", 0, None).unwrap();
        let second = parse_reader(
            &mut Cursor::new(&second_chunk),
            "rollout-a",
            first.consumed_bytes,
            Some(first.checkpoint),
        )
        .unwrap();

        assert_eq!(first.outcome.events.len(), 1);
        assert_eq!(second.outcome.events.len(), 1);
        assert_eq!(second.outcome.events[0].tokens.total(), 18);
        assert!(
            second.outcome.events[0]
                .source_event_key
                .ends_with(&first.consumed_bytes.to_string())
        );
    }

    #[test]
    fn file_entrypoint_returns_metadata_and_the_same_normalized_event() {
        let bytes = jsonl(&[
            meta("session-file", None),
            turn("turn-file", "gpt-test"),
            usage("2026-08-01T00:00:02Z", [10, 0, 0, 2, 1, 12], None),
        ]);
        let path = std::env::temp_dir().join(format!(
            "yaat-codex-usage-{}-{}.jsonl",
            std::process::id(),
            now_ms()
        ));
        fs::write(&path, &bytes).expect("write fixture");

        let result = parse_bytes(&fs::read(&path).expect("read fixture"));
        let _ = fs::remove_file(&path);

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].session_id, "session-file");
    }

    #[test]
    fn ignores_record_bodies_and_reports_only_complete_malformed_records() {
        let mut bytes = jsonl(&[
            meta("session-a", None),
            turn("turn-a", "gpt-test"),
            r#"{"type":"response_item","payload":{"type":"message","content":"private body"}}"#
                .to_owned(),
            "not json".to_owned(),
            usage("2026-08-01T00:00:02Z", [10, 0, 0, 2, 1, 12], None),
        ]);
        bytes.extend_from_slice(b"{\"type\":\"event_msg\"");

        let result = parse_bytes(&bytes);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.diagnostics.malformed_records, 1);
        assert!(result.diagnostics.is_partial);
    }
}
