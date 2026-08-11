//! Additive, local-only session history reconciliation.
//!
//! Missing sessions and strict extensions are synchronized without following
//! discovered symlinks. Divergent histories are reported as conflicts and are
//! never overwritten.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use directories::BaseDirs;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use uuid::Uuid;
use walkdir::WalkDir;
use yaat_contracts::{
    HistoryApplyResult, HistoryGroup, HistoryPreview, HistoryScope, OperationPhase,
    OperationProgress,
};

use crate::activation::{FileFingerprint, replace_atomically};
use crate::db::{HistorySourceState, Repository};
use crate::error::{AppError, AppResult};

pub const CODEX_HISTORY_PROVIDER_ID: &str = "custom";

const MAX_HISTORY_FILE_BYTES: u64 = 512 * 1024 * 1024;
const CLAUDE_SESSIONS_DIR: &str = "claude-code-sessions";

#[derive(Clone, Debug)]
pub(crate) struct HistoryRoot {
    pub id: String,
    pub label: String,
    pub root_kind: String,
    pub path: PathBuf,
    pub is_current: bool,
}

#[derive(Clone)]
struct FileSnapshot {
    root_index: usize,
    path: PathBuf,
    relative_path: PathBuf,
    key: String,
    normalized_fingerprint: Option<FileFingerprint>,
    needs_metadata_update: bool,
}

struct ScanState {
    scope: HistoryScope,
    roots: Vec<HistoryRoot>,
    files: Vec<FileSnapshot>,
    invalid_files: u64,
}

#[derive(Clone, Copy)]
struct HistoryIndex<'a> {
    repository: Option<&'a Repository>,
    use_cache: bool,
}

#[derive(Default)]
struct CopyPlan {
    copies: Vec<CopyAction>,
    identical_files: u64,
    conflicts: u64,
}

struct CopyAction {
    source_index: usize,
    target: CopyTarget,
}

enum CopyTarget {
    ExistingFile(usize),
    MissingRoot(usize),
}

#[cfg(test)]
pub(crate) fn preview(
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
) -> AppResult<HistoryPreview> {
    preview_cancellable(
        scope,
        supplied_roots,
        target_group_id,
        &AtomicBool::new(false),
        |_| {},
    )
}

pub(crate) fn preview_cancellable(
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(OperationProgress),
) -> AppResult<HistoryPreview> {
    progress(OperationProgress {
        phase: OperationPhase::Discovering,
        processed: 0,
        total: None,
    });
    let roots = match scope {
        HistoryScope::Codex | HistoryScope::ClaudeCode => supplied_roots,
        HistoryScope::ClaudeDesktopCode => {
            claude_desktop_groups_cancellable(supplied_roots, cancelled)?
        }
    };
    let state = scan_cancellable(scope, roots, None, cancelled, &mut progress)?;
    let groups = groups_from_scan(&state);

    if scope == HistoryScope::ClaudeDesktopCode && target_group_id.is_none() {
        return Ok(HistoryPreview {
            scope,
            groups,
            files_scanned: state.files.len() as u64,
            invalid_files: state.invalid_files,
            ..Default::default()
        });
    }

    let target_index = resolve_target(&state, target_group_id)?;
    check_cancelled(cancelled)?;
    let plan = plan_copies(&state, target_index)?;
    Ok(HistoryPreview {
        scope,
        groups,
        target_group_id: target_group_id.map(str::to_owned),
        files_scanned: state.files.len() as u64,
        pending_copies: plan.copies.len() as u64,
        metadata_updates: state
            .files
            .iter()
            .filter(|file| file.needs_metadata_update)
            .count() as u64,
        identical_files: plan.identical_files,
        conflicts: plan.conflicts,
        invalid_files: state.invalid_files,
    })
}

#[cfg(test)]
pub(crate) fn apply(
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
) -> AppResult<HistoryApplyResult> {
    apply_cancellable(
        scope,
        supplied_roots,
        target_group_id,
        &AtomicBool::new(false),
        |_| {},
    )
}

#[cfg(test)]
pub(crate) fn apply_cancellable(
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
    cancelled: &AtomicBool,
    progress: impl FnMut(OperationProgress),
) -> AppResult<HistoryApplyResult> {
    apply_with_repository(
        HistoryIndex {
            repository: None,
            use_cache: false,
        },
        scope,
        supplied_roots,
        target_group_id,
        cancelled,
        || Ok(()),
        progress,
    )
}

pub(crate) fn apply_incremental_cancellable(
    repository: &Repository,
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
    cancelled: &AtomicBool,
    before_write: impl FnMut() -> AppResult<()>,
    progress: impl FnMut(OperationProgress),
) -> AppResult<HistoryApplyResult> {
    apply_with_repository(
        HistoryIndex {
            repository: Some(repository),
            use_cache: true,
        },
        scope,
        supplied_roots,
        target_group_id,
        cancelled,
        before_write,
        progress,
    )
}

pub(crate) fn apply_full_indexed_cancellable(
    repository: &Repository,
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
    cancelled: &AtomicBool,
    before_write: impl FnMut() -> AppResult<()>,
    progress: impl FnMut(OperationProgress),
) -> AppResult<HistoryApplyResult> {
    apply_with_repository(
        HistoryIndex {
            repository: Some(repository),
            use_cache: false,
        },
        scope,
        supplied_roots,
        target_group_id,
        cancelled,
        before_write,
        progress,
    )
}

fn apply_with_repository(
    index: HistoryIndex<'_>,
    scope: HistoryScope,
    supplied_roots: Vec<HistoryRoot>,
    target_group_id: Option<&str>,
    cancelled: &AtomicBool,
    mut before_write: impl FnMut() -> AppResult<()>,
    mut progress: impl FnMut(OperationProgress),
) -> AppResult<HistoryApplyResult> {
    progress(OperationProgress {
        phase: OperationPhase::Discovering,
        processed: 0,
        total: None,
    });
    let roots = match scope {
        HistoryScope::Codex | HistoryScope::ClaudeCode => supplied_roots,
        HistoryScope::ClaudeDesktopCode => {
            claude_desktop_groups_cancellable(supplied_roots, cancelled)?
        }
    };
    let repository = index.repository;
    let cache = repository
        .filter(|_| index.use_cache)
        .map(|repository| load_history_cache(repository, scope))
        .transpose()?;
    let mut state = scan_cancellable(scope, roots, cache.as_ref(), cancelled, &mut progress)?;
    let target_index = resolve_target(&state, target_group_id)?;
    let initial_plan = plan_copies(&state, target_index)?;
    let needs_metadata_update =
        scope == HistoryScope::Codex && state.files.iter().any(|file| file.needs_metadata_update);
    let needs_state_database_update = scope == HistoryScope::Codex
        && codex_state_databases_need_normalization(&state.roots, cancelled)?;

    if !needs_metadata_update && !needs_state_database_update && initial_plan.copies.is_empty() {
        if let Some(repository) = repository {
            save_history_cache(repository, &state)?;
        }
        return Ok(HistoryApplyResult {
            scope,
            copied: 0,
            metadata_updated: 0,
            identical_files: initial_plan.identical_files,
            conflicts: initial_plan.conflicts,
            invalid_files: state.invalid_files,
        });
    }

    // Discovery and planning are read-only and remain safe while the client is
    // running. Enforce process preconditions only when the plan actually has
    // something to write.
    check_cancelled(cancelled)?;
    before_write()?;

    let mut metadata_updated = 0u64;
    if scope == HistoryScope::Codex {
        progress(OperationProgress {
            phase: OperationPhase::Saving,
            processed: 0,
            total: Some(state.files.len() as u64),
        });
        for file in &state.files {
            check_cancelled(cancelled)?;
            if !file.needs_metadata_update {
                continue;
            }
            let replacement = codex_replacement(&file.path)?.ok_or_else(|| {
                AppError::Internal("Codex history metadata changed during apply".into())
            })?;
            replace_atomically(&file.path, &replacement)
                .map_err(|error| AppError::ConfigConflict(error.to_string()))?;
            metadata_updated = metadata_updated.saturating_add(1);
        }
        metadata_updated = metadata_updated
            .saturating_add(normalize_codex_state_databases(&state.roots, cancelled)?);
        // Re-scan after the provider-only rewrites so copies use the exact bytes
        // that are now present on disk.
        state = scan_cancellable(scope, state.roots, cache.as_ref(), cancelled, &mut progress)?;
    }

    check_cancelled(cancelled)?;
    let plan = plan_copies(&state, target_index)?;
    let mut copied = 0u64;
    progress(OperationProgress {
        phase: OperationPhase::Saving,
        processed: 0,
        total: Some(plan.copies.len() as u64),
    });
    for action in &plan.copies {
        check_cancelled(cancelled)?;
        let source = &state.files[action.source_index];
        let (target_root, target) = match action.target {
            CopyTarget::ExistingFile(target_index) => {
                let target = &state.files[target_index];
                (&state.roots[target.root_index], target.path.clone())
            }
            CopyTarget::MissingRoot(target_root_index) => {
                let target_root = &state.roots[target_root_index];
                (target_root, target_root.path.join(&source.relative_path))
            }
        };
        ensure_safe_parent(&target_root.path, &target)?;
        let replacement = history_replacement(scope, source, &target)?;
        replace_atomically(&target, &replacement)
            .map_err(|error| AppError::ConfigConflict(error.to_string()))?;
        copied = copied.saturating_add(1);
        progress(OperationProgress {
            phase: OperationPhase::Saving,
            processed: copied,
            total: Some(plan.copies.len() as u64),
        });
    }

    if let Some(repository) = repository {
        save_history_cache(repository, &state)?;
    }
    Ok(HistoryApplyResult {
        scope,
        copied,
        metadata_updated,
        identical_files: plan.identical_files,
        conflicts: plan.conflicts,
        invalid_files: state.invalid_files,
    })
}

fn normalize_codex_state_databases(
    roots: &[HistoryRoot],
    cancelled: &AtomicBool,
) -> AppResult<u64> {
    let mut updated = 0_u64;
    for root in roots {
        check_cancelled(cancelled)?;
        let path = root.path.join("state_5.sqlite");
        if !path.exists() {
            continue;
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            AppError::ConfigConflict(format!(
                "unable to open Codex state database {}: {error}",
                path.display()
            ))
        })?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| AppError::ConfigConflict(error.to_string()))?;
        let changed = connection
            .execute(
                "UPDATE threads SET model_provider = ?1 WHERE model_provider <> ?1 OR model_provider IS NULL",
                [CODEX_HISTORY_PROVIDER_ID],
            )
            .map_err(|error| {
                AppError::ConfigConflict(format!(
                    "unable to normalize Codex state database {}: {error}",
                    path.display()
                ))
            })?;
        updated = updated.saturating_add(changed as u64);
    }
    Ok(updated)
}

fn codex_state_databases_need_normalization(
    roots: &[HistoryRoot],
    cancelled: &AtomicBool,
) -> AppResult<bool> {
    for root in roots {
        check_cancelled(cancelled)?;
        let path = root.path.join("state_5.sqlite");
        if !path.exists() {
            continue;
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            AppError::ConfigConflict(format!(
                "unable to inspect Codex state database {}: {error}",
                path.display()
            ))
        })?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| AppError::ConfigConflict(error.to_string()))?;
        let needs_update = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM threads WHERE model_provider <> ?1 OR model_provider IS NULL LIMIT 1)",
                [CODEX_HISTORY_PROVIDER_ID],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| {
                AppError::ConfigConflict(format!(
                    "unable to inspect Codex state database {}: {error}",
                    path.display()
                ))
            })?;
        if needs_update {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
fn scan(scope: HistoryScope, roots: Vec<HistoryRoot>) -> AppResult<ScanState> {
    scan_cancellable(scope, roots, None, &AtomicBool::new(false), &mut |_| {})
}

fn scan_cancellable(
    scope: HistoryScope,
    roots: Vec<HistoryRoot>,
    cache: Option<&HashMap<(String, String), HistorySourceState>>,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(OperationProgress),
) -> AppResult<ScanState> {
    let mut files = Vec::new();
    let mut invalid_files = 0u64;
    progress(OperationProgress {
        phase: OperationPhase::Processing,
        processed: 0,
        total: None,
    });
    for (root_index, root) in roots.iter().enumerate() {
        check_cancelled(cancelled)?;
        match scope {
            HistoryScope::Codex => {
                for base_name in ["sessions", "archived_sessions"] {
                    let base = root.path.join(base_name);
                    if !base.is_dir() {
                        continue;
                    }
                    for entry in WalkDir::new(&base).follow_links(false) {
                        check_cancelled(cancelled)?;
                        let entry = entry.map_err(AppError::io)?;
                        if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy();
                        if !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")) {
                            continue;
                        }
                        let relative = entry
                            .path()
                            .strip_prefix(&root.path)
                            .map_err(|_| {
                                AppError::Internal("history path escaped its root".into())
                            })?
                            .to_path_buf();
                        let snapshot = match cached_snapshot(
                            cache,
                            scope,
                            root_index,
                            root,
                            entry.path(),
                            &relative,
                        )? {
                            Some(snapshot) => snapshot,
                            None => codex_snapshot(root_index, entry.path(), relative)?,
                        };
                        if snapshot.normalized_fingerprint.is_none() {
                            invalid_files = invalid_files.saturating_add(1);
                        }
                        files.push(snapshot);
                        progress(OperationProgress {
                            phase: OperationPhase::Processing,
                            processed: files.len() as u64,
                            total: None,
                        });
                    }
                }
            }
            HistoryScope::ClaudeCode => {
                let base = root.path.join("projects");
                if !base.is_dir() {
                    continue;
                }
                for entry in WalkDir::new(&base).follow_links(false) {
                    check_cancelled(cancelled)?;
                    let entry = entry.map_err(AppError::io)?;
                    if !entry.file_type().is_file()
                        || entry.path().extension().and_then(|value| value.to_str())
                            != Some("jsonl")
                    {
                        continue;
                    }
                    let relative = entry
                        .path()
                        .strip_prefix(&root.path)
                        .map_err(|_| AppError::Internal("history path escaped its root".into()))?
                        .to_path_buf();
                    let snapshot = match cached_snapshot(
                        cache,
                        scope,
                        root_index,
                        root,
                        entry.path(),
                        &relative,
                    )? {
                        Some(snapshot) => snapshot,
                        None => claude_code_snapshot(root_index, entry.path(), relative)?,
                    };
                    files.push(snapshot);
                    progress(OperationProgress {
                        phase: OperationPhase::Processing,
                        processed: files.len() as u64,
                        total: None,
                    });
                }
            }
            HistoryScope::ClaudeDesktopCode => {
                if !root.path.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(&root.path).map_err(AppError::io)? {
                    check_cancelled(cancelled)?;
                    let entry = entry.map_err(AppError::io)?;
                    let file_type = entry.file_type().map_err(AppError::io)?;
                    if file_type.is_symlink() || !file_type.is_file() {
                        continue;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if !name.starts_with("local_")
                        || !Path::new(name)
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                    {
                        continue;
                    }
                    let relative = PathBuf::from(name);
                    let snapshot = match cached_snapshot(
                        cache,
                        scope,
                        root_index,
                        root,
                        &entry.path(),
                        &relative,
                    )? {
                        Some(snapshot) => snapshot,
                        None => claude_snapshot(root_index, &entry.path(), relative)?,
                    };
                    if snapshot.normalized_fingerprint.is_none() {
                        invalid_files = invalid_files.saturating_add(1);
                    }
                    files.push(snapshot);
                    progress(OperationProgress {
                        phase: OperationPhase::Processing,
                        processed: files.len() as u64,
                        total: None,
                    });
                }
            }
        }
    }
    files.sort_by(|left, right| {
        left.root_index
            .cmp(&right.root_index)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    progress(OperationProgress {
        phase: OperationPhase::Processing,
        processed: files.len() as u64,
        total: Some(files.len() as u64),
    });
    Ok(ScanState {
        scope,
        roots,
        files,
        invalid_files,
    })
}

fn load_history_cache(
    repository: &Repository,
    scope: HistoryScope,
) -> AppResult<HashMap<(String, String), HistorySourceState>> {
    Ok(repository
        .history_source_states(scope)
        .map_err(AppError::database)?
        .into_iter()
        .map(|state| ((state.root_id.clone(), state.source_path.clone()), state))
        .collect())
}

fn cached_snapshot(
    cache: Option<&HashMap<(String, String), HistorySourceState>>,
    _scope: HistoryScope,
    root_index: usize,
    root: &HistoryRoot,
    path: &Path,
    relative_path: &Path,
) -> AppResult<Option<FileSnapshot>> {
    let Some(cache) = cache else {
        return Ok(None);
    };
    let source_path = path.to_string_lossy().into_owned();
    let Some(cached) = cache.get(&(root.id.clone(), source_path)) else {
        return Ok(None);
    };
    let metadata = fs::metadata(path).map_err(AppError::io)?;
    let modified_at = modified_millis(&metadata)?;
    if metadata.len() != cached.file_size || modified_at != cached.modified_at {
        return Ok(None);
    }
    let Some(fingerprint) = FileFingerprint::from_hex(&cached.fingerprint) else {
        return Ok(None);
    };
    Ok(Some(FileSnapshot {
        root_index,
        path: path.to_path_buf(),
        relative_path: relative_path.to_path_buf(),
        key: cached.session_key.clone(),
        normalized_fingerprint: Some(fingerprint),
        needs_metadata_update: false,
    }))
}

fn save_history_cache(repository: &Repository, state: &ScanState) -> AppResult<()> {
    let sources = state
        .files
        .iter()
        .filter_map(|file| {
            let fingerprint = file.normalized_fingerprint?;
            let metadata = fs::metadata(&file.path).ok()?;
            let modified_at = modified_millis(&metadata).ok()?;
            Some(HistorySourceState {
                root_id: state.roots[file.root_index].id.clone(),
                source_path: file.path.to_string_lossy().into_owned(),
                session_key: file.key.clone(),
                file_size: metadata.len(),
                modified_at,
                fingerprint: fingerprint.to_hex(),
            })
        })
        .collect::<Vec<_>>();
    repository
        .replace_history_sources(state.scope, &sources)
        .map_err(AppError::database)
}

fn modified_millis(metadata: &fs::Metadata) -> AppResult<i64> {
    let millis = metadata
        .modified()
        .map_err(AppError::io)?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AppError::Validation("history timestamp predates the epoch".into()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| AppError::Validation("history timestamp is out of range".into()))
}

fn claude_code_snapshot(
    root_index: usize,
    path: &Path,
    relative_path: PathBuf,
) -> AppResult<FileSnapshot> {
    let raw = read_regular_file(path)?;
    let fingerprint = FileFingerprint::from_bytes(&raw);
    Ok(FileSnapshot {
        root_index,
        path: path.to_path_buf(),
        key: relative_path.to_string_lossy().into_owned(),
        relative_path,
        normalized_fingerprint: Some(fingerprint),
        needs_metadata_update: false,
    })
}

fn codex_snapshot(
    root_index: usize,
    path: &Path,
    relative_path: PathBuf,
) -> AppResult<FileSnapshot> {
    let raw = read_regular_file(path)?;
    let compressed = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"));
    let decoded = if compressed {
        decode_zstd_limited(&raw).ok()
    } else {
        Some(raw)
    };
    let normalized = decoded
        .as_deref()
        .and_then(|bytes| normalize_codex_jsonl(bytes).ok());
    let normalized_fingerprint = normalized.as_deref().map(FileFingerprint::from_bytes);
    let needs_metadata_update = normalized
        .as_deref()
        .is_some_and(|normalized| decoded.as_deref() != Some(normalized));
    let key = decoded
        .as_deref()
        .and_then(codex_session_id)
        .unwrap_or(canonical_codex_key(path)?);
    Ok(FileSnapshot {
        root_index,
        path: path.to_path_buf(),
        relative_path,
        key,
        normalized_fingerprint,
        needs_metadata_update,
    })
}

fn codex_replacement(path: &Path) -> AppResult<Option<Vec<u8>>> {
    let raw = read_regular_file(path)?;
    let compressed = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"));
    let decoded = if compressed {
        decode_zstd_limited(&raw)?
    } else {
        raw
    };
    let normalized = normalize_codex_jsonl(&decoded)
        .map_err(|_| AppError::Validation("Codex session metadata is malformed".into()))?;
    if normalized == decoded {
        return Ok(None);
    }
    if compressed {
        zstd::stream::encode_all(Cursor::new(normalized), 0)
            .map(Some)
            .map_err(AppError::io)
    } else {
        Ok(Some(normalized))
    }
}

fn claude_snapshot(
    root_index: usize,
    path: &Path,
    relative_path: PathBuf,
) -> AppResult<FileSnapshot> {
    let raw = read_regular_file(path)?;
    let normalized = serde_json::from_slice::<Value>(&raw)
        .ok()
        .filter(Value::is_object)
        .and_then(|value| serde_json::to_vec(&value).ok());
    let normalized_fingerprint = normalized.as_deref().map(FileFingerprint::from_bytes);
    let key = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::Validation("Claude session file name is not UTF-8".into()))?
        .to_owned();
    Ok(FileSnapshot {
        root_index,
        path: path.to_path_buf(),
        relative_path,
        key,
        normalized_fingerprint,
        needs_metadata_update: false,
    })
}

fn normalize_codex_jsonl(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let mut output = Vec::with_capacity(bytes.len());
    let mut saw_session_meta = false;

    for segment in text.split_inclusive('\n') {
        let (body_with_cr, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |body| (body, "\n"));
        let (body, carriage_return) = body_with_cr
            .strip_suffix('\r')
            .map_or((body_with_cr, ""), |body| (body, "\r"));
        let relevant =
            body.contains("\"session_meta\"") || body.contains("\"thread_settings_applied\"");
        if !relevant {
            output.extend_from_slice(segment.as_bytes());
            continue;
        }

        let mut value: Value = serde_json::from_str(body).map_err(|_| ())?;
        let mut changed = false;
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                saw_session_meta = true;
                let payload = value
                    .get_mut("payload")
                    .and_then(Value::as_object_mut)
                    .ok_or(())?;
                if payload.get("model_provider").and_then(Value::as_str)
                    != Some(CODEX_HISTORY_PROVIDER_ID)
                {
                    payload.insert(
                        "model_provider".into(),
                        Value::String(CODEX_HISTORY_PROVIDER_ID.into()),
                    );
                    changed = true;
                }
            }
            Some("event_msg")
                if value
                    .get("payload")
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("thread_settings_applied") =>
            {
                let settings = value
                    .get_mut("payload")
                    .and_then(|payload| payload.get_mut("thread_settings"))
                    .and_then(Value::as_object_mut)
                    .ok_or(())?;
                if settings.get("model_provider_id").and_then(Value::as_str)
                    != Some(CODEX_HISTORY_PROVIDER_ID)
                {
                    settings.insert(
                        "model_provider_id".into(),
                        Value::String(CODEX_HISTORY_PROVIDER_ID.into()),
                    );
                    changed = true;
                }
            }
            _ => {}
        }

        if changed {
            output.extend_from_slice(serde_json::to_string(&value).map_err(|_| ())?.as_bytes());
            output.extend_from_slice(carriage_return.as_bytes());
            output.extend_from_slice(newline.as_bytes());
        } else {
            output.extend_from_slice(segment.as_bytes());
        }
    }

    saw_session_meta.then_some(output).ok_or(())
}

fn codex_session_id(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        if !line.contains("\"session_meta\"") {
            continue;
        }
        let value: Value = serde_json::from_str(line).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let id = value
            .get("payload")
            .and_then(|payload| payload.get("id"))
            .and_then(Value::as_str)?
            .trim();
        if !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control) {
            return Some(id.to_owned());
        }
    }
    None
}

fn plan_copies(state: &ScanState, target_index: Option<usize>) -> AppResult<CopyPlan> {
    let mut by_key: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, file) in state.files.iter().enumerate() {
        by_key.entry(&file.key).or_default().push(index);
    }

    let mut plan = CopyPlan::default();
    for indexes in by_key.values() {
        if indexes
            .iter()
            .any(|index| state.files[*index].normalized_fingerprint.is_none())
        {
            plan.conflicts = plan.conflicts.saturating_add(1);
            continue;
        }
        let fingerprints = indexes
            .iter()
            .filter_map(|index| state.files[*index].normalized_fingerprint)
            .collect::<BTreeSet<_>>();
        let source_index = if fingerprints.len() == 1 {
            indexes[0]
        } else if let Some(source_index) = extension_source(state, indexes)? {
            source_index
        } else {
            plan.conflicts = plan.conflicts.saturating_add(1);
            continue;
        };
        let mut counts = BTreeMap::<FileFingerprint, usize>::new();
        for index in indexes {
            let fingerprint = state.files[*index]
                .normalized_fingerprint
                .expect("invalid history files were rejected above");
            *counts.entry(fingerprint).or_default() += 1;
        }
        plan.identical_files = plan.identical_files.saturating_add(
            counts
                .values()
                .map(|count| count.saturating_sub(1) as u64)
                .sum(),
        );
        let source_fingerprint = state.files[source_index]
            .normalized_fingerprint
            .expect("the selected source is valid");
        let existing_roots = indexes
            .iter()
            .map(|index| state.files[*index].root_index)
            .collect::<BTreeSet<_>>();
        match state.scope {
            HistoryScope::Codex | HistoryScope::ClaudeCode => {
                for index in indexes {
                    if state.files[*index].normalized_fingerprint != Some(source_fingerprint) {
                        plan.copies.push(CopyAction {
                            source_index,
                            target: CopyTarget::ExistingFile(*index),
                        });
                    }
                }
                for root_index in 0..state.roots.len() {
                    if !existing_roots.contains(&root_index) {
                        plan.copies.push(CopyAction {
                            source_index,
                            target: CopyTarget::MissingRoot(root_index),
                        });
                    }
                }
            }
            HistoryScope::ClaudeDesktopCode => {
                if let Some(target_root_index) = target_index {
                    let target_files = indexes
                        .iter()
                        .copied()
                        .filter(|index| state.files[*index].root_index == target_root_index)
                        .collect::<Vec<_>>();
                    if target_files.is_empty() {
                        plan.copies.push(CopyAction {
                            source_index,
                            target: CopyTarget::MissingRoot(target_root_index),
                        });
                    } else {
                        for index in target_files {
                            if state.files[index].normalized_fingerprint != Some(source_fingerprint)
                            {
                                plan.copies.push(CopyAction {
                                    source_index,
                                    target: CopyTarget::ExistingFile(index),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(plan)
}

fn extension_source(state: &ScanState, indexes: &[usize]) -> AppResult<Option<usize>> {
    let contents = indexes
        .iter()
        .map(|index| {
            Ok((
                *index,
                normalized_history_bytes(state.scope, &state.files[*index])?,
            ))
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(contents
        .iter()
        .filter(|(_, candidate)| {
            contents
                .iter()
                .all(|(_, base)| base == candidate || history_extends(state.scope, base, candidate))
        })
        .max_by_key(|(_, candidate)| candidate.len())
        .map(|(index, _)| *index))
}

fn normalized_history_bytes(scope: HistoryScope, file: &FileSnapshot) -> AppResult<Vec<u8>> {
    let raw = read_regular_file(&file.path)?;
    match scope {
        HistoryScope::Codex => {
            let decoded = if is_zstd_history(&file.path) {
                decode_zstd_limited(&raw)?
            } else {
                raw
            };
            normalize_codex_jsonl(&decoded)
                .map_err(|_| AppError::Validation("Codex session metadata is malformed".into()))
        }
        HistoryScope::ClaudeCode => Ok(raw),
        HistoryScope::ClaudeDesktopCode => {
            let value: Value = serde_json::from_slice(&raw)
                .map_err(|_| AppError::Validation("Claude session file is malformed".into()))?;
            if !value.is_object() {
                return Err(AppError::Validation(
                    "Claude session file root must be an object".into(),
                ));
            }
            serde_json::to_vec(&value).map_err(|error| AppError::Internal(error.to_string()))
        }
    }
}

fn history_extends(scope: HistoryScope, base: &[u8], candidate: &[u8]) -> bool {
    match scope {
        HistoryScope::Codex | HistoryScope::ClaudeCode => {
            candidate.len() > base.len()
                && candidate.starts_with(base)
                && (base.is_empty() || base.ends_with(b"\n"))
        }
        HistoryScope::ClaudeDesktopCode => {
            let Ok(base) = serde_json::from_slice::<Value>(base) else {
                return false;
            };
            let Ok(candidate) = serde_json::from_slice::<Value>(candidate) else {
                return false;
            };
            base != candidate && json_is_extension(&base, &candidate)
        }
    }
}

fn json_is_extension(base: &Value, candidate: &Value) -> bool {
    match (base, candidate) {
        (Value::Object(base), Value::Object(candidate)) => base.iter().all(|(key, value)| {
            candidate
                .get(key)
                .is_some_and(|candidate| json_is_extension(value, candidate))
        }),
        (Value::Array(base), Value::Array(candidate)) => {
            base.len() <= candidate.len()
                && base
                    .iter()
                    .zip(candidate)
                    .all(|(base, candidate)| json_is_extension(base, candidate))
        }
        _ => base == candidate,
    }
}

fn history_replacement(
    scope: HistoryScope,
    source: &FileSnapshot,
    target: &Path,
) -> AppResult<Vec<u8>> {
    if scope != HistoryScope::Codex {
        return read_regular_file(&source.path);
    }
    let normalized = normalized_history_bytes(scope, source)?;
    if is_zstd_history(target) {
        zstd::stream::encode_all(Cursor::new(normalized), 0).map_err(AppError::io)
    } else {
        Ok(normalized)
    }
}

fn is_zstd_history(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

fn groups_from_scan(state: &ScanState) -> Vec<HistoryGroup> {
    state
        .roots
        .iter()
        .enumerate()
        .map(|(index, root)| HistoryGroup {
            id: root.id.clone(),
            label: root.label.clone(),
            root_kind: root.root_kind.clone(),
            is_current: root.is_current,
            session_count: state
                .files
                .iter()
                .filter(|file| file.root_index == index)
                .count() as u64,
        })
        .collect()
}

fn resolve_target(state: &ScanState, target_group_id: Option<&str>) -> AppResult<Option<usize>> {
    match state.scope {
        HistoryScope::Codex | HistoryScope::ClaudeCode => Ok(None),
        HistoryScope::ClaudeDesktopCode => {
            let id = target_group_id.ok_or_else(|| {
                AppError::Validation(
                    "select a Claude Desktop target account and organization".into(),
                )
            })?;
            state
                .roots
                .iter()
                .position(|root| root.id == id)
                .map(Some)
                .ok_or_else(|| {
                    AppError::ConfigConflict(
                        "the selected Claude Desktop account group no longer exists".into(),
                    )
                })
        }
    }
}

fn read_regular_file(path: &Path) -> AppResult<Vec<u8>> {
    let metadata = fs::metadata(path).map_err(AppError::io)?;
    if !metadata.file_type().is_file() {
        return Err(AppError::Validation(format!(
            "refusing to read non-regular session file {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_HISTORY_FILE_BYTES {
        return Err(AppError::Validation(format!(
            "session file is larger than the supported limit: {}",
            path.display()
        )));
    }
    fs::read(path).map_err(AppError::io)
}

fn decode_zstd_limited(raw: &[u8]) -> AppResult<Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(raw)).map_err(AppError::io)?;
    let mut decoded = Vec::new();
    decoder
        .take(MAX_HISTORY_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut decoded)
        .map_err(AppError::io)?;
    if decoded.len() as u64 > MAX_HISTORY_FILE_BYTES {
        return Err(AppError::Validation(
            "decompressed Codex session exceeds the supported limit".into(),
        ));
    }
    Ok(decoded)
}

fn canonical_codex_key(path: &Path) -> AppResult<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::Validation("Codex session file name is not UTF-8".into()))?;
    Ok(name.strip_suffix(".zst").unwrap_or(name).to_owned())
}

fn ensure_safe_parent(root: &Path, target: &Path) -> AppResult<()> {
    if !root.is_absolute() || !target.starts_with(root) {
        return Err(AppError::Validation(
            "history destination escaped its configured root".into(),
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| AppError::Validation("history destination has no parent".into()))?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| AppError::Validation("history destination escaped its root".into()))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::Validation(
            "history destination contains an invalid path component".into(),
        ));
    }
    fs::create_dir_all(parent).map_err(AppError::io)?;
    if !parent.is_dir() {
        return Err(AppError::Validation(format!(
            "history destination parent is not a directory: {}",
            parent.display()
        )));
    }
    Ok(())
}

fn claude_desktop_groups_cancellable(
    mut managed_roots: Vec<HistoryRoot>,
    cancelled: &AtomicBool,
) -> AppResult<Vec<HistoryRoot>> {
    let base = BaseDirs::new()
        .ok_or_else(|| AppError::Io("unable to resolve Claude Desktop data directory".into()))?;
    #[cfg(target_os = "macos")]
    let data_parent = base.data_dir().to_path_buf();
    #[cfg(target_os = "windows")]
    let data_parent = base.data_local_dir().to_path_buf();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let data_parent = base.data_local_dir().to_path_buf();

    let normal_path = data_parent.join("Claude");
    let threep_path = data_parent.join("Claude-3p");
    let managed_selected = managed_roots.iter().any(|root| root.is_current);
    let system_threep_selected = read_deployment_mode(&normal_path).as_deref() == Some("3p");
    let mut roots = vec![
        HistoryRoot {
            id: "claude".into(),
            label: "Claude".into(),
            root_kind: "global".into(),
            path: normal_path,
            is_current: !managed_selected && !system_threep_selected,
        },
        HistoryRoot {
            id: "claude_3p".into(),
            label: "Claude-3p".into(),
            root_kind: "global_3p".into(),
            path: threep_path,
            is_current: !managed_selected && system_threep_selected,
        },
    ];
    roots.append(&mut managed_roots);
    claude_desktop_groups_at_cancellable(&roots, cancelled)
}

#[cfg(test)]
fn claude_desktop_groups_at(roots: &[HistoryRoot]) -> AppResult<Vec<HistoryRoot>> {
    claude_desktop_groups_at_cancellable(roots, &AtomicBool::new(false))
}

fn claude_desktop_groups_at_cancellable(
    roots: &[HistoryRoot],
    cancelled: &AtomicBool,
) -> AppResult<Vec<HistoryRoot>> {
    let mut groups = Vec::new();
    for data_root in roots {
        check_cancelled(cancelled)?;
        let current_account = read_current_claude_account(&data_root.path);
        let sessions_root = data_root.path.join(CLAUDE_SESSIONS_DIR);
        if !sessions_root.is_dir() {
            continue;
        }
        for account_entry in fs::read_dir(&sessions_root).map_err(AppError::io)? {
            check_cancelled(cancelled)?;
            let account_entry = account_entry.map_err(AppError::io)?;
            if account_entry
                .file_type()
                .map_err(AppError::io)?
                .is_symlink()
                || !account_entry.file_type().map_err(AppError::io)?.is_dir()
            {
                continue;
            }
            let Some(account_raw) = account_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(account_uuid) = Uuid::parse_str(&account_raw) else {
                continue;
            };
            for org_entry in fs::read_dir(account_entry.path()).map_err(AppError::io)? {
                check_cancelled(cancelled)?;
                let org_entry = org_entry.map_err(AppError::io)?;
                let file_type = org_entry.file_type().map_err(AppError::io)?;
                if file_type.is_symlink() || !file_type.is_dir() {
                    continue;
                }
                let Some(org_raw) = org_entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(org_uuid) = Uuid::parse_str(&org_raw) else {
                    continue;
                };
                let account = account_uuid.to_string();
                let org = org_uuid.to_string();
                groups.push(HistoryRoot {
                    id: format!("{}:{account}:{org}", data_root.id),
                    label: format!("{} · {} / {}", data_root.label, &account[..8], &org[..8]),
                    root_kind: data_root.root_kind.clone(),
                    path: org_entry.path(),
                    is_current: data_root.is_current
                        && current_account
                            .as_ref()
                            .is_some_and(|current| current == &account_uuid),
                });
            }
        }
    }
    groups.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(groups)
}

fn check_cancelled(cancelled: &AtomicBool) -> AppResult<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}

fn read_current_claude_account(data_root: &Path) -> Option<Uuid> {
    let bytes = fs::read(data_root.join("config.json")).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("lastKnownAccountUuid")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn read_deployment_mode(data_root: &Path) -> Option<String> {
    let bytes = fs::read(data_root.join("claude_desktop_config.json")).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("deploymentMode")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_preview_stops_before_reading_history() {
        let cancelled = AtomicBool::new(true);
        let error = preview_cancellable(
            HistoryScope::Codex,
            vec![root("global", tempfile::tempdir().unwrap().path())],
            None,
            &cancelled,
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, AppError::Cancelled));
    }

    fn root(id: &str, path: &Path) -> HistoryRoot {
        HistoryRoot {
            id: id.into(),
            label: id.into(),
            root_kind: "test".into(),
            path: path.to_path_buf(),
            is_current: false,
        }
    }

    fn codex_line(provider: &str) -> Vec<u8> {
        format!(
            "{{\"timestamp\":\"now\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"1\",\"model_provider\":\"{provider}\"}}}}\n\
{{\"timestamp\":\"now\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"thread_settings_applied\",\"thread_settings\":{{\"model_provider_id\":\"{provider}\"}}}}}}\n"
        )
        .into_bytes()
    }

    #[test]
    fn codex_normalization_updates_every_provider_metadata_source() {
        let normalized = normalize_codex_jsonl(&codex_line("openai")).unwrap();
        let text = String::from_utf8(normalized).unwrap();
        assert_eq!(text.matches(CODEX_HISTORY_PROVIDER_ID).count(), 2);
        assert!(!text.contains("\"openai\""));
    }

    #[test]
    fn write_guard_runs_after_read_only_history_scan() {
        use std::cell::Cell;

        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("sessions/2026/08/03/rollout-test.jsonl");
        let source = first.join(relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, codex_line("openai")).unwrap();
        let scan_completed = Cell::new(false);

        let error = apply_with_repository(
            HistoryIndex {
                repository: None,
                use_cache: false,
            },
            HistoryScope::Codex,
            vec![root("first", &first), root("second", &second)],
            None,
            &AtomicBool::new(false),
            || {
                assert!(scan_completed.get());
                Err(AppError::ConfigConflict("client is running".into()))
            },
            |progress| {
                if progress.phase == OperationPhase::Processing && progress.processed > 0 {
                    scan_completed.set(true);
                }
            },
        )
        .unwrap_err();

        assert!(matches!(error, AppError::ConfigConflict(_)));
        assert!(fs::read_to_string(source).unwrap().contains("\"openai\""));
        assert!(!second.join(relative).exists());
    }

    #[test]
    fn read_only_history_scan_does_not_run_write_guard_when_nothing_changed() {
        use std::cell::Cell;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("only");
        let source = root_path.join("sessions/2026/08/03/rollout-test.jsonl");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, codex_line(CODEX_HISTORY_PROVIDER_ID)).unwrap();
        let guard_called = Cell::new(false);

        let result = apply_with_repository(
            HistoryIndex {
                repository: None,
                use_cache: false,
            },
            HistoryScope::Codex,
            vec![root("only", &root_path)],
            None,
            &AtomicBool::new(false),
            || {
                guard_called.set(true);
                Err(AppError::ConfigConflict("client is running".into()))
            },
            |_| {},
        )
        .unwrap();

        assert!(!guard_called.get());
        assert_eq!(result.copied, 0);
        assert_eq!(result.metadata_updated, 0);
    }

    #[test]
    fn codex_apply_copies_without_touching_unrelated_lines() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("sessions/2026/08/03/rollout-test.jsonl");
        let source = first.join(relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let mut bytes = codex_line("openai");
        bytes.extend_from_slice(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"keep me\"}}\n");
        fs::write(&source, bytes).unwrap();

        let roots = vec![root("first", &first), root("second", &second)];
        let preview = preview(HistoryScope::Codex, roots.clone(), None).unwrap();
        assert_eq!(preview.pending_copies, 1);
        assert_eq!(preview.metadata_updates, 1);
        let result = apply(HistoryScope::Codex, roots, None).unwrap();
        assert_eq!(result.copied, 1);
        assert_eq!(result.metadata_updated, 1);
        let copied = fs::read_to_string(second.join(relative)).unwrap();
        assert!(copied.contains("keep me"));
        assert!(copied.contains(CODEX_HISTORY_PROVIDER_ID));
    }

    #[test]
    fn codex_conflict_never_overwrites_target() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("sessions/2026/08/03/rollout-test.jsonl");
        for (root, suffix) in [(&first, "one"), (&second, "two")] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut bytes = codex_line("openai");
            bytes.extend_from_slice(format!("{{\"suffix\":\"{suffix}\"}}\n").as_bytes());
            fs::write(path, bytes).unwrap();
        }
        let roots = vec![root("first", &first), root("second", &second)];
        let preview = preview(HistoryScope::Codex, roots.clone(), None).unwrap();
        assert_eq!(preview.conflicts, 1);
        assert_eq!(preview.pending_copies, 0);
        let result = apply(HistoryScope::Codex, roots, None).unwrap();
        assert_eq!(result.copied, 0);
        let after = fs::read_to_string(second.join(relative)).unwrap();
        assert!(after.contains("\"suffix\":\"two\""));
        assert!(!after.contains("\"suffix\":\"one\""));
    }

    #[test]
    fn claude_code_apply_copies_projects_additively() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("projects/-Users-haozi-Code-yaat/session.jsonl");
        let source = first.join(relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"{\"type\":\"user\",\"message\":\"keep me\"}\n").unwrap();

        let roots = vec![root("first", &first), root("second", &second)];
        let preview = preview(HistoryScope::ClaudeCode, roots.clone(), None).unwrap();
        assert_eq!(preview.pending_copies, 1);
        assert_eq!(preview.metadata_updates, 0);

        let result = apply(HistoryScope::ClaudeCode, roots, None).unwrap();
        assert_eq!(result.copied, 1);
        assert_eq!(
            fs::read(second.join(relative)).unwrap(),
            fs::read(source).unwrap()
        );
    }

    #[test]
    fn claude_groups_reject_non_uuid_path_components() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Claude");
        fs::create_dir_all(
            data.join(CLAUDE_SESSIONS_DIR)
                .join("../not-an-account")
                .join("not-an-org"),
        )
        .unwrap();
        let groups = claude_desktop_groups_at(&[HistoryRoot {
            id: "claude".into(),
            label: "Claude".into(),
            root_kind: "test".into(),
            path: data,
            is_current: true,
        }])
        .unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn claude_groups_include_managed_desktop_profiles_and_copy_additively() {
        let temp = tempfile::tempdir().unwrap();
        let account = "11111111-1111-4111-8111-111111111111";
        let org = "22222222-2222-4222-8222-222222222222";
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let first_sessions = first.join(CLAUDE_SESSIONS_DIR).join(account).join(org);
        let second_sessions = second.join(CLAUDE_SESSIONS_DIR).join(account).join(org);
        fs::create_dir_all(&first_sessions).unwrap();
        fs::create_dir_all(&second_sessions).unwrap();
        fs::write(
            first.join("config.json"),
            format!(r#"{{"lastKnownAccountUuid":"{account}"}}"#),
        )
        .unwrap();
        fs::write(
            second.join("config.json"),
            format!(r#"{{"lastKnownAccountUuid":"{account}"}}"#),
        )
        .unwrap();
        fs::write(
            first_sessions.join("local_session.json"),
            br#"{"title":"keep"}"#,
        )
        .unwrap();

        let descriptors = vec![
            HistoryRoot {
                id: "profile:first".into(),
                label: "First Desktop".into(),
                root_kind: "managed".into(),
                path: first,
                is_current: false,
            },
            HistoryRoot {
                id: "profile:second".into(),
                label: "Second Desktop".into(),
                root_kind: "managed".into(),
                path: second,
                is_current: true,
            },
        ];
        let groups = claude_desktop_groups_at(&descriptors).unwrap();
        assert_eq!(groups.len(), 2);
        assert!(
            groups
                .iter()
                .any(|group| group.label.starts_with("First Desktop"))
        );
        let target = groups
            .iter()
            .find(|group| group.is_current)
            .unwrap()
            .id
            .clone();
        let state = scan(HistoryScope::ClaudeDesktopCode, groups.clone()).unwrap();
        let target_index = resolve_target(&state, Some(&target)).unwrap();
        assert_eq!(plan_copies(&state, target_index).unwrap().copies.len(), 1);
    }

    #[test]
    fn codex_apply_updates_a_strictly_extended_session() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("sessions/2026/08/03/rollout-test.jsonl");
        let first_path = first.join(relative);
        let second_path = second.join(relative);
        fs::create_dir_all(first_path.parent().unwrap()).unwrap();
        fs::create_dir_all(second_path.parent().unwrap()).unwrap();
        let old = codex_line(CODEX_HISTORY_PROVIDER_ID);
        let mut extended = old.clone();
        extended.extend_from_slice(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"continued\"}}\n");
        fs::write(&first_path, &extended).unwrap();
        fs::write(&second_path, &old).unwrap();

        let roots = vec![root("first", &first), root("second", &second)];
        let preview = preview(HistoryScope::Codex, roots.clone(), None).unwrap();
        assert_eq!(preview.pending_copies, 1);
        assert_eq!(preview.conflicts, 0);

        let result = apply(HistoryScope::Codex, roots, None).unwrap();
        assert_eq!(result.copied, 1);
        assert_eq!(fs::read(second_path).unwrap(), extended);
    }

    #[test]
    fn codex_session_id_is_stable_across_different_rollout_paths() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let first_path = first.join("sessions/2026/08/03/rollout-new-name.jsonl");
        let second_path = second.join("archived_sessions/rollout-old-name.jsonl");
        fs::create_dir_all(first_path.parent().unwrap()).unwrap();
        fs::create_dir_all(second_path.parent().unwrap()).unwrap();
        let old = codex_line(CODEX_HISTORY_PROVIDER_ID);
        let mut extended = old.clone();
        extended.extend_from_slice(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"continued\"}}\n",
        );
        fs::write(&first_path, &extended).unwrap();
        fs::write(&second_path, &old).unwrap();

        let result = apply(
            HistoryScope::Codex,
            vec![root("first", &first), root("second", &second)],
            None,
        )
        .unwrap();

        assert_eq!(result.copied, 1);
        assert_eq!(result.conflicts, 0);
        assert_eq!(fs::read(second_path).unwrap(), extended);
    }

    #[test]
    fn claude_code_apply_updates_a_strictly_extended_session() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let relative = Path::new("projects/-workspace/session.jsonl");
        let first_path = first.join(relative);
        let second_path = second.join(relative);
        fs::create_dir_all(first_path.parent().unwrap()).unwrap();
        fs::create_dir_all(second_path.parent().unwrap()).unwrap();
        let old = b"{\"type\":\"user\",\"message\":\"first\"}\n";
        let mut extended = old.to_vec();
        extended.extend_from_slice(b"{\"type\":\"assistant\",\"message\":\"continued\"}\n");
        fs::write(&first_path, &extended).unwrap();
        fs::write(&second_path, old).unwrap();

        let roots = vec![root("first", &first), root("second", &second)];
        let result = apply(HistoryScope::ClaudeCode, roots, None).unwrap();

        assert_eq!(result.copied, 1);
        assert_eq!(result.conflicts, 0);
        assert_eq!(fs::read(second_path).unwrap(), extended);
    }

    #[test]
    fn claude_desktop_updates_a_target_with_an_appended_session() {
        let temp = tempfile::tempdir().unwrap();
        let account = "11111111-1111-4111-8111-111111111111";
        let org = "22222222-2222-4222-8222-222222222222";
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let first_sessions = first.join(CLAUDE_SESSIONS_DIR).join(account).join(org);
        let second_sessions = second.join(CLAUDE_SESSIONS_DIR).join(account).join(org);
        fs::create_dir_all(&first_sessions).unwrap();
        fs::create_dir_all(&second_sessions).unwrap();
        for root in [&first, &second] {
            fs::write(
                root.join("config.json"),
                format!(r#"{{"lastKnownAccountUuid":"{account}"}}"#),
            )
            .unwrap();
        }
        fs::write(
            first_sessions.join("local_session.json"),
            br#"{"messages":[{"id":"one"},{"id":"two"}]}"#,
        )
        .unwrap();
        fs::write(
            second_sessions.join("local_session.json"),
            br#"{"messages":[{"id":"one"}]}"#,
        )
        .unwrap();
        let descriptors = vec![
            HistoryRoot {
                id: "profile:first".into(),
                label: "First Desktop".into(),
                root_kind: "managed".into(),
                path: first,
                is_current: false,
            },
            HistoryRoot {
                id: "profile:second".into(),
                label: "Second Desktop".into(),
                root_kind: "managed".into(),
                path: second,
                is_current: true,
            },
        ];
        let groups = claude_desktop_groups_at(&descriptors).unwrap();
        let target = groups
            .iter()
            .find(|group| group.is_current)
            .unwrap()
            .id
            .clone();
        let state = scan(HistoryScope::ClaudeDesktopCode, groups).unwrap();
        let target_index = resolve_target(&state, Some(&target)).unwrap();
        let plan = plan_copies(&state, target_index).unwrap();

        assert_eq!(plan.copies.len(), 1);
        assert_eq!(plan.conflicts, 0);
        let action = &plan.copies[0];
        assert!(matches!(action.target, CopyTarget::ExistingFile(_)));
    }
}
