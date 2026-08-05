//! Local-only usage indexing and date-bucket reporting.
//!
//! This service never contacts a provider. It scans metadata-only JSONL parsers,
//! persists normalized token counters, and stores no prompts or responses.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use walkdir::WalkDir;
use yaat_contracts::{
    OperationPhase, OperationProgress, Platform, TokenBreakdown, UsageBucket, UsageDiagnostics,
    UsageQueryRequest, UsageReport,
};

use crate::db::{Repository, UsageRecordInput, UsageScanSummary};
use crate::error::{AppError, AppResult};
use crate::usage::UsageEventDraft;

use super::{claude, codex};

const MAX_USAGE_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_QUERY_DAYS: i64 = 366;

#[derive(Clone, Debug)]
pub struct UsageRoot {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub indexed: usize,
    pub diagnostics: UsageDiagnostics,
}

#[cfg(test)]
pub fn scan(repo: &Repository, platform: Platform, roots: &[UsageRoot]) -> AppResult<ScanSummary> {
    scan_cancellable(repo, platform, roots, &AtomicBool::new(false), |_| {})
}

pub fn scan_cancellable(
    repo: &Repository,
    platform: Platform,
    roots: &[UsageRoot],
    cancelled: &AtomicBool,
    mut progress: impl FnMut(OperationProgress),
) -> AppResult<ScanSummary> {
    let roots = unique_roots(roots);
    let mut diagnostics = UsageDiagnostics::default();
    let mut records = Vec::new();

    progress(OperationProgress {
        phase: OperationPhase::Discovering,
        processed: 0,
        total: None,
    });

    match platform {
        Platform::Codex => scan_codex(
            &roots,
            &mut records,
            &mut diagnostics,
            cancelled,
            &mut progress,
        )?,
        Platform::ClaudeCode => scan_claude(
            &roots,
            &mut records,
            &mut diagnostics,
            cancelled,
            &mut progress,
        )?,
        Platform::ClaudeDesktop => {
            return Err(AppError::Validation(
                "Claude Desktop local token indexing is not supported yet".into(),
            ));
        }
    }

    check_cancelled(cancelled)?;
    progress(OperationProgress {
        phase: OperationPhase::Saving,
        processed: diagnostics.files_scanned,
        total: Some(diagnostics.files_scanned),
    });

    let indexed = repo
        .replace_usage_snapshot(platform, &records)
        .map_err(AppError::database)?;
    let state = UsageScanSummary {
        platform,
        diagnostics: diagnostics.clone(),
    };
    repo.save_usage_scan_summary(&state)
        .map_err(AppError::database)?;

    Ok(ScanSummary {
        indexed,
        diagnostics,
    })
}

pub fn query(repo: &Repository, request: &UsageQueryRequest) -> AppResult<UsageReport> {
    let timezone = Tz::from_str(request.timezone.trim())
        .map_err(|_| AppError::Validation("unknown IANA timezone".into()))?;
    let start_date = parse_date(&request.start_date)?;
    let end_date = parse_date(&request.end_date)?;
    if end_date < start_date {
        return Err(AppError::Validation(
            "usage end date must not precede start date".into(),
        ));
    }
    let days = end_date.signed_duration_since(start_date).num_days() + 1;
    if days > MAX_QUERY_DAYS {
        return Err(AppError::Validation(format!(
            "usage date range must not exceed {MAX_QUERY_DAYS} days"
        )));
    }

    let start_at = local_day_start(timezone, start_date)?;
    let exclusive_end_date = end_date
        .succ_opt()
        .ok_or_else(|| AppError::Validation("usage end date is out of range".into()))?;
    let end_at = local_day_start(timezone, exclusive_end_date)?;
    let rows = repo
        .usage_rows(request.platform, start_at, end_at)
        .map_err(AppError::database)?;

    let mut buckets = BTreeMap::<String, UsageBucket>::new();
    let mut date = start_date;
    while date <= end_date {
        let key = date.format("%Y-%m-%d").to_string();
        buckets.insert(
            key.clone(),
            UsageBucket {
                date: key,
                ..UsageBucket::default()
            },
        );
        date = date
            .succ_opt()
            .ok_or_else(|| AppError::Validation("usage date is out of range".into()))?;
    }

    let mut totals = TokenBreakdown::default();
    let mut request_count = 0_u64;
    for row in rows {
        add_tokens(&mut totals, &row.tokens);
        request_count = request_count.saturating_add(row.request_count);
        let local = Utc
            .timestamp_millis_opt(row.occurred_at)
            .single()
            .ok_or_else(|| AppError::Database("usage timestamp is out of range".into()))?
            .with_timezone(&timezone);
        let key = local.date_naive().format("%Y-%m-%d").to_string();
        if let Some(bucket) = buckets.get_mut(&key) {
            add_tokens(&mut bucket.tokens, &row.tokens);
            bucket.request_count = bucket.request_count.saturating_add(row.request_count);
        }
    }

    let diagnostics = repo
        .load_usage_scan_summary(request.platform)
        .map_err(AppError::database)?
        .map_or_else(UsageDiagnostics::default, |state| state.diagnostics);

    Ok(UsageReport {
        platform: request.platform,
        start_date: request.start_date.clone(),
        end_date: request.end_date.clone(),
        timezone: request.timezone.clone(),
        totals,
        request_count,
        buckets: buckets.into_values().collect(),
        diagnostics,
    })
}

fn scan_codex(
    roots: &[UsageRoot],
    records: &mut Vec<UsageRecordInput>,
    diagnostics: &mut UsageDiagnostics,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(OperationProgress),
) -> AppResult<()> {
    let mut paths = Vec::new();
    for root in roots {
        for directory in [
            root.path.join("sessions"),
            root.path.join("archived_sessions"),
        ] {
            paths.extend(jsonl_files_cancellable(&directory, true, cancelled)?);
        }
    }
    let total = paths.len() as u64;
    progress(OperationProgress {
        phase: OperationPhase::Processing,
        processed: 0,
        total: Some(total),
    });
    for (index, path) in paths.into_iter().enumerate() {
        check_cancelled(cancelled)?;
        let parsed = if path.to_string_lossy().ends_with(".jsonl.zst") {
            read_zstd_limited(&path).map(|bytes| codex::parse_bytes(&bytes))
        } else {
            read_usage_source_limited(&path).map(|bytes| codex::parse_bytes(&bytes))
        };
        match parsed {
            Ok(parsed) => {
                merge_diagnostics(diagnostics, &parsed.diagnostics);
                records.extend(parsed.events.into_iter().map(usage_record));
            }
            Err(_) => {
                diagnostics.files_scanned = diagnostics.files_scanned.saturating_add(1);
                diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
                diagnostics.is_partial = true;
            }
        }
        progress(OperationProgress {
            phase: OperationPhase::Processing,
            processed: index as u64 + 1,
            total: Some(total),
        });
    }
    Ok(())
}

fn scan_claude(
    roots: &[UsageRoot],
    records: &mut Vec<UsageRecordInput>,
    diagnostics: &mut UsageDiagnostics,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(OperationProgress),
) -> AppResult<()> {
    let mut events = Vec::new();
    let mut sources = Vec::new();

    for root in roots {
        let projects = root.path.join("projects");
        for path in jsonl_files_cancellable(&projects, false, cancelled)? {
            sources.push((path, root.path.clone()));
        }
    }

    let total = sources.len() as u64;
    progress(OperationProgress {
        phase: OperationPhase::Processing,
        processed: 0,
        total: Some(total),
    });
    for (index, (path, root)) in sources.into_iter().enumerate() {
        check_cancelled(cancelled)?;
        let bytes = match read_usage_source_limited(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                diagnostics.files_scanned = diagnostics.files_scanned.saturating_add(1);
                diagnostics.malformed_records = diagnostics.malformed_records.saturating_add(1);
                diagnostics.is_partial = true;
                progress(OperationProgress {
                    phase: OperationPhase::Processing,
                    processed: index as u64 + 1,
                    total: Some(total),
                });
                continue;
            }
        };
        let source = claude::ClaudeSource::from_path(&path, &root);
        let outcome = claude::parse_jsonl(&bytes, &source);
        merge_diagnostics(diagnostics, &outcome.diagnostics);
        events.extend(outcome.events);
        progress(OperationProgress {
            phase: OperationPhase::Processing,
            processed: index as u64 + 1,
            total: Some(total),
        });
    }

    check_cancelled(cancelled)?;
    let reconciled = claude::reconcile_events(events);
    diagnostics.duplicate_records = diagnostics
        .duplicate_records
        .saturating_add(reconciled.duplicate_records);
    records.extend(reconciled.events.into_iter().map(usage_record));
    Ok(())
}

fn jsonl_files_cancellable(
    root: &Path,
    include_zstd: bool,
    cancelled: &AtomicBool,
) -> AppResult<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        check_cancelled(cancelled)?;
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            || (include_zstd && path.to_string_lossy().ends_with(".jsonl.zst"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
fn jsonl_files(root: &Path, include_zstd: bool) -> Vec<PathBuf> {
    jsonl_files_cancellable(root, include_zstd, &AtomicBool::new(false)).unwrap()
}

fn check_cancelled(cancelled: &AtomicBool) -> AppResult<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}

fn read_usage_source_limited(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_USAGE_SOURCE_BYTES {
        return Err("usage source exceeds the supported size or is not a file".into());
    }
    fs::read(path).map_err(|error| error.to_string())
}

fn read_zstd_limited(path: &Path) -> Result<Vec<u8>, String> {
    let compressed = read_usage_source_limited(path)?;
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(compressed))
        .map_err(|error| error.to_string())?;
    let mut decoded = Vec::new();
    decoder
        .take(MAX_USAGE_SOURCE_BYTES + 1)
        .read_to_end(&mut decoded)
        .map_err(|error| error.to_string())?;
    if decoded.len() as u64 > MAX_USAGE_SOURCE_BYTES {
        return Err("compressed usage source exceeds the supported size".into());
    }
    Ok(decoded)
}

fn unique_roots(roots: &[UsageRoot]) -> Vec<UsageRoot> {
    let mut seen = HashSet::new();
    roots
        .iter()
        .filter(|root| seen.insert(root.path.clone()))
        .cloned()
        .collect()
}

fn usage_record(event: UsageEventDraft) -> UsageRecordInput {
    UsageRecordInput {
        event_id: event.source_event_key,
        platform: event.platform,
        occurred_at: event.occurred_at_ms,
        tokens: event.tokens,
        request_count: event.request_count,
    }
}

fn merge_diagnostics(target: &mut UsageDiagnostics, source: &UsageDiagnostics) {
    target.files_scanned = target.files_scanned.saturating_add(source.files_scanned);
    target.malformed_records = target
        .malformed_records
        .saturating_add(source.malformed_records);
    target.duplicate_records = target
        .duplicate_records
        .saturating_add(source.duplicate_records);
    target.coverage_start = min_optional(target.coverage_start, source.coverage_start);
    target.coverage_end = max_optional(target.coverage_end, source.coverage_end);
    target.last_scanned_at = max_optional(target.last_scanned_at, source.last_scanned_at);
    target.is_partial |= source.is_partial;
}

fn min_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn max_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

fn parse_date(value: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| AppError::Validation("date must use YYYY-MM-DD".into()))
}

fn local_day_start(timezone: Tz, date: NaiveDate) -> AppResult<i64> {
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| AppError::Validation("date is out of range".into()))?;
    let local = match timezone.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, second) => first.min(second),
        LocalResult::None => {
            return Err(AppError::Validation(
                "the selected timezone has no local midnight for this date".into(),
            ));
        }
    };
    Ok(local.with_timezone(&Utc).timestamp_millis())
}

fn add_tokens(target: &mut TokenBreakdown, source: &TokenBreakdown) {
    target.uncached_input = target.uncached_input.saturating_add(source.uncached_input);
    target.cache_read = target.cache_read.saturating_add(source.cache_read);
    target.cache_write = target.cache_write.saturating_add(source.cache_write);
    target.output = target.output.saturating_add(source.output);
    target.reasoning_output = target
        .reasoning_output
        .saturating_add(source.reasoning_output);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_scan_does_not_publish_a_snapshot() {
        let repository = Repository::in_memory().unwrap();
        let cancelled = AtomicBool::new(true);
        let error =
            scan_cancellable(&repository, Platform::Codex, &[], &cancelled, |_| {}).unwrap_err();

        assert!(matches!(error, AppError::Cancelled));
        assert!(
            repository
                .load_usage_scan_summary(Platform::Codex)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn codex_usage_sources_include_bounded_zstd_archives() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout.jsonl.zst");
        let content = b"{\"type\":\"event_msg\"}\n";
        fs::write(
            &path,
            zstd::stream::encode_all(Cursor::new(content), 0).unwrap(),
        )
        .unwrap();

        let included = jsonl_files(temp.path(), true);
        assert_eq!(included, vec![path.clone()]);
        assert_eq!(read_zstd_limited(&path).unwrap(), content);
        let excluded = jsonl_files(temp.path(), false);
        assert!(excluded.is_empty());
    }

    #[test]
    fn usage_source_reader_rejects_oversized_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("oversized.jsonl");
        let handle = fs::File::create(&file).unwrap();
        handle.set_len(MAX_USAGE_SOURCE_BYTES + 1).unwrap();
        assert!(read_usage_source_limited(&file).is_err());
    }

    #[test]
    fn claude_history_copies_are_counted_once_across_profile_roots() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("projects/-workspace/session-a.jsonl");
        let transcript = concat!(
            r#"{"type":"assistant","sessionId":"session-a","timestamp":"2026-04-05T12:00:00Z","requestId":"req-1","message":{"id":"msg-1","stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":5}}}"#,
            "\n"
        );
        for root in [&first, &second] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, transcript).unwrap();
        }
        let repository = Repository::in_memory().unwrap();

        let summary = scan(
            &repository,
            Platform::ClaudeCode,
            &[UsageRoot { path: first }, UsageRoot { path: second }],
        )
        .unwrap();
        let rows = repository
            .usage_rows(Platform::ClaudeCode, 0, i64::MAX)
            .unwrap();

        assert_eq!(summary.indexed, 1);
        assert_eq!(summary.diagnostics.duplicate_records, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens.uncached_input, 3);
        assert_eq!(rows[0].tokens.output, 5);
    }
}
