//! Format-aware, owned-path configuration patching and rollback.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{JsonValue, ParseOptions, parse_to_value};
use serde_json::Value as JsonValueOwned;
use thiserror::Error;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value as TomlValue};

use super::atomic::{
    AtomicWriteError, ExpectedFileState, FileFingerprint, observed_state, read_file,
    remove_atomically, replace_atomically, replace_atomically_if_unchanged,
};

const UTF8_BOM: &[u8; 3] = b"\xef\xbb\xbf";
const MAX_CONFIG_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_SEGMENTS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;

/// Configuration syntax understood by the patch engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigFormat {
    Toml,
    Json,
    Jsonc,
}

impl ConfigFormat {
    const fn name(self) -> &'static str {
        match self {
            Self::Toml => "TOML",
            Self::Json => "JSON",
            Self::Jsonc => "JSONC",
        }
    }
}

/// A logical object/table path owned by YAAT.
///
/// Paths contain object keys only (no array indices). They are rendered as
/// RFC 6901 JSON Pointers for persistence and diagnostics, regardless of the
/// underlying config format.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OwnedPath(Vec<String>);

impl OwnedPath {
    /// Builds a path from decoded object-key segments.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::InvalidOwnedPath`] when the path is empty, too
    /// large, or contains an empty or control-character segment.
    pub fn from_segments<I, S>(segments: I) -> Result<Self, PatchError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let segments = segments.into_iter().map(Into::into).collect::<Vec<_>>();
        validate_path_segments(&segments)?;
        Ok(Self(segments))
    }

    /// Parses a non-root RFC 6901 JSON Pointer.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::InvalidOwnedPath`] for invalid pointer escapes or
    /// for a path that violates the owned-path limits.
    pub fn from_json_pointer(pointer: &str) -> Result<Self, PatchError> {
        if !pointer.starts_with('/') {
            return Err(PatchError::InvalidOwnedPath {
                path: pointer.to_owned(),
                reason: "a non-root JSON Pointer must start with '/'",
            });
        }
        let mut segments = Vec::new();
        for raw_segment in pointer[1..].split('/') {
            let mut segment = String::with_capacity(raw_segment.len());
            let mut chars = raw_segment.chars();
            while let Some(character) = chars.next() {
                if character != '~' {
                    segment.push(character);
                    continue;
                }
                match chars.next() {
                    Some('0') => segment.push('~'),
                    Some('1') => segment.push('/'),
                    _ => {
                        return Err(PatchError::InvalidOwnedPath {
                            path: pointer.to_owned(),
                            reason: "invalid JSON Pointer escape",
                        });
                    }
                }
            }
            segments.push(segment);
        }
        validate_path_segments(&segments)?;
        Ok(Self(segments))
    }

    /// Returns the decoded object-key segments.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.0
    }

    /// Renders the path as an RFC 6901 JSON Pointer.
    #[must_use]
    pub fn to_json_pointer(&self) -> String {
        let mut pointer = String::new();
        for segment in &self.0 {
            pointer.push('/');
            pointer.push_str(&segment.replace('~', "~0").replace('/', "~1"));
        }
        pointer
    }

    fn is_prefix_of(&self, other: &Self) -> bool {
        self.0.len() <= other.0.len() && self.0.iter().zip(&other.0).all(|(a, b)| a == b)
    }

    fn is_related_to(&self, other: &Self) -> bool {
        self.is_prefix_of(other) || other.is_prefix_of(self)
    }
}

impl fmt::Debug for OwnedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_json_pointer())
    }
}

impl fmt::Display for OwnedPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_json_pointer())
    }
}

impl FromStr for OwnedPath {
    type Err = PatchError;

    fn from_str(pointer: &str) -> Result<Self, Self::Err> {
        Self::from_json_pointer(pointer)
    }
}

/// A redacted set or remove operation for one owned path.
#[derive(Clone, Eq, PartialEq)]
pub enum PatchOperation {
    Set {
        path: OwnedPath,
        value: JsonValueOwned,
    },
    Remove {
        path: OwnedPath,
    },
}

impl PatchOperation {
    /// Creates an operation that sets an owned path to a JSON-compatible value.
    #[must_use]
    pub fn set(path: OwnedPath, value: impl Into<JsonValueOwned>) -> Self {
        Self::Set {
            path,
            value: value.into(),
        }
    }

    /// Creates an operation that removes an owned path when it exists.
    #[must_use]
    pub const fn remove(path: OwnedPath) -> Self {
        Self::Remove { path }
    }

    /// Returns the path affected by this operation.
    #[must_use]
    pub fn path(&self) -> &OwnedPath {
        match self {
            Self::Set { path, .. } | Self::Remove { path } => path,
        }
    }
}

impl fmt::Debug for PatchOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set { path, .. } => formatter
                .debug_struct("Set")
                .field("path", path)
                .field("value", &"<redacted>")
                .finish(),
            Self::Remove { path } => formatter
                .debug_struct("Remove")
                .field("path", path)
                .finish(),
        }
    }
}

/// Redacted semantic value observed at an owned path.
#[derive(Clone, Eq, PartialEq)]
pub struct PathState {
    pub exists: bool,
    pub value: Option<JsonValueOwned>,
}

impl PathState {
    /// Returns the state of a path that does not exist.
    #[must_use]
    pub const fn missing() -> Self {
        Self {
            exists: false,
            value: None,
        }
    }

    fn from_semantic(value: Option<&SemanticValue>) -> Self {
        value.map_or_else(Self::missing, |value| Self {
            exists: true,
            value: Some(value.to_public_json()),
        })
    }
}

impl fmt::Debug for PathState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PathState")
            .field("exists", &self.exists)
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Before and after state recorded for one changed path.
#[derive(Clone, Eq, PartialEq)]
pub struct PathChange {
    pub path: OwnedPath,
    pub before: PathState,
    pub after: PathState,
}

impl fmt::Debug for PathChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PathChange")
            .field("path", &self.path)
            .field("before", &self.before)
            .field("after", &self.after)
            .finish()
    }
}

/// Parsed, size-bounded snapshot used to prepare a patch.
pub struct ConfigSnapshot {
    path: PathBuf,
    format: ConfigFormat,
    state: ExpectedFileState,
    bytes: Vec<u8>,
    materialize: bool,
    rollback: RollbackStrategy,
}

#[derive(Clone)]
enum RollbackStrategy {
    Paths,
    WholeFile { previous: Option<Vec<u8>> },
}

impl ConfigSnapshot {
    /// Reads and validates a configuration snapshot from an absolute path.
    ///
    /// # Errors
    ///
    /// Returns an error for relative paths, unsupported files, oversized input,
    /// invalid UTF-8, or malformed configuration syntax.
    pub fn read(path: impl Into<PathBuf>, format: ConfigFormat) -> Result<Self, PatchError> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(PatchError::RelativeConfigPath { path });
        }
        let maybe_bytes = read_file(&path)?;
        let (state, bytes) = match maybe_bytes {
            Some(bytes) => {
                if bytes.len() > MAX_CONFIG_BYTES {
                    return Err(PatchError::ConfigTooLarge {
                        path,
                        bytes: bytes.len(),
                        maximum: MAX_CONFIG_BYTES,
                    });
                }
                (
                    ExpectedFileState::Present(FileFingerprint::from_bytes(&bytes)),
                    bytes,
                )
            }
            None => (ExpectedFileState::Missing, Vec::new()),
        };
        // Parse now, not only during rendering, so callers never hold a
        // seemingly valid snapshot of malformed input.
        parse_semantic(&bytes, format, state != ExpectedFileState::Missing)?;
        Ok(Self {
            path,
            format,
            state,
            bytes,
            materialize: false,
            rollback: RollbackStrategy::Paths,
        })
    }

    /// Creates a snapshot for a target that must not exist, using `base` as
    /// the byte-preserving seed document.
    ///
    /// This is intended for the first creation of a managed profile: an adapter
    /// can copy the user's source config as a base and patch only its owned
    /// account paths without ever rewriting the source file.
    ///
    /// # Errors
    ///
    /// Returns an error when the target is not an absolute missing path or the
    /// seed document is oversized, invalid UTF-8, or malformed.
    pub fn for_missing_target_with_base(
        target: impl Into<PathBuf>,
        format: ConfigFormat,
        base: impl Into<Vec<u8>>,
    ) -> Result<Self, PatchError> {
        let path = target.into();
        if !path.is_absolute() {
            return Err(PatchError::RelativeConfigPath { path });
        }
        let observed = observed_state(&path)?;
        if observed != ExpectedFileState::Missing {
            return Err(AtomicWriteError::Conflict {
                expected: ExpectedFileState::Missing,
                observed,
            }
            .into());
        }
        let bytes = base.into();
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(PatchError::ConfigTooLarge {
                path,
                bytes: bytes.len(),
                maximum: MAX_CONFIG_BYTES,
            });
        }
        decode_utf8(&bytes, &path)?;
        parse_semantic(&bytes, format, true)?;
        Ok(Self {
            path,
            format,
            state: ExpectedFileState::Missing,
            bytes,
            materialize: true,
            rollback: RollbackStrategy::WholeFile { previous: None },
        })
    }

    /// Returns the target configuration path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the target configuration format.
    #[must_use]
    pub const fn format(&self) -> ConfigFormat {
        self.format
    }

    /// Reports whether the target existed when the snapshot was taken.
    #[must_use]
    pub const fn existed(&self) -> bool {
        matches!(self.state, ExpectedFileState::Present(_))
    }

    /// Returns the original file fingerprint, if the target existed.
    #[must_use]
    pub const fn fingerprint(&self) -> Option<FileFingerprint> {
        self.state.fingerprint()
    }
}

impl fmt::Debug for ConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigSnapshot")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("state", &self.state)
            .field(
                "bytes",
                &format_args!("<redacted; {} bytes>", self.bytes.len()),
            )
            .finish()
    }
}

/// Validated patch ready for compare-and-swap publication.
pub struct PreparedPatch {
    path: PathBuf,
    format: ConfigFormat,
    expected: ExpectedFileState,
    replacement: Vec<u8>,
    changes: Vec<PathChange>,
    changed_paths: Vec<OwnedPath>,
    requires_write: bool,
    rollback: RollbackStrategy,
}

impl PreparedPatch {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn format(&self) -> ConfigFormat {
        self.format
    }

    #[must_use]
    pub const fn before_fingerprint(&self) -> Option<FileFingerprint> {
        self.expected.fingerprint()
    }

    #[must_use]
    pub const fn before_existed(&self) -> bool {
        matches!(self.expected, ExpectedFileState::Present(_))
    }

    #[must_use]
    pub fn proposed_fingerprint(&self) -> Option<FileFingerprint> {
        if self.requires_write {
            Some(FileFingerprint::from_bytes(&self.replacement))
        } else {
            self.expected.fingerprint()
        }
    }

    #[must_use]
    pub fn changes(&self) -> &[PathChange] {
        &self.changes
    }

    #[must_use]
    pub fn semantic_changed_paths(&self) -> &[OwnedPath] {
        &self.changed_paths
    }

    #[must_use]
    pub const fn is_noop(&self) -> bool {
        !self.requires_write
    }
}

impl fmt::Debug for PreparedPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedPatch")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("expected", &self.expected)
            .field(
                "replacement",
                &format_args!("<redacted; {} bytes>", self.replacement.len()),
            )
            .field("changes", &self.changes)
            .field("semantic_changed_paths", &self.changed_paths)
            .field("requires_write", &self.requires_write)
            .finish()
    }
}

#[derive(Clone)]
/// Receipt for a committed patch, including the state required for rollback.
pub struct AppliedPatch {
    path: PathBuf,
    format: ConfigFormat,
    before: ExpectedFileState,
    after: ExpectedFileState,
    changes: Vec<PathChange>,
    wrote_file: bool,
    rollback: RollbackStrategy,
}

impl AppliedPatch {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn format(&self) -> ConfigFormat {
        self.format
    }

    #[must_use]
    pub const fn before_existed(&self) -> bool {
        matches!(self.before, ExpectedFileState::Present(_))
    }

    #[must_use]
    pub const fn before_fingerprint(&self) -> Option<FileFingerprint> {
        self.before.fingerprint()
    }

    #[must_use]
    pub const fn after_fingerprint(&self) -> Option<FileFingerprint> {
        self.after.fingerprint()
    }

    #[must_use]
    pub fn changes(&self) -> &[PathChange] {
        &self.changes
    }

    #[must_use]
    pub const fn wrote_file(&self) -> bool {
        self.wrote_file
    }
}

impl fmt::Debug for AppliedPatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppliedPatch")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("before", &self.before)
            .field("after", &self.after)
            .field("changes", &self.changes)
            .field("wrote_file", &self.wrote_file)
            .finish()
    }
}

/// Result of attempting to restore a previously applied patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RollbackOutcome {
    Restored { fingerprint: FileFingerprint },
    RemovedFileCreatedByPatch,
    AlreadyRolledBack,
    Conflict { paths: Vec<OwnedPath> },
}

/// Stateless entry point for preparing, committing, and rolling back patches.
pub struct PatchEngine;

impl PatchEngine {
    pub fn snapshot(
        path: impl Into<PathBuf>,
        format: ConfigFormat,
    ) -> Result<ConfigSnapshot, PatchError> {
        ConfigSnapshot::read(path, format)
    }

    pub fn prepare(
        snapshot: ConfigSnapshot,
        operations: Vec<PatchOperation>,
    ) -> Result<PreparedPatch, PatchError> {
        validate_operations(&operations)?;
        let before_semantic = parse_semantic(&snapshot.bytes, snapshot.format, snapshot.existed())?;
        let replacement = render_patch(&snapshot, &operations)?;
        if replacement.len() > MAX_CONFIG_BYTES {
            return Err(PatchError::ConfigTooLarge {
                path: snapshot.path,
                bytes: replacement.len(),
                maximum: MAX_CONFIG_BYTES,
            });
        }
        let after_semantic = parse_semantic(&replacement, snapshot.format, true)?;
        let owned_paths = operations
            .iter()
            .map(|operation| operation.path().clone())
            .collect::<Vec<_>>();
        let changed_paths = semantic_diff(&before_semantic, &after_semantic);
        let disallowed = changed_paths
            .iter()
            .filter(|changed| !owned_paths.iter().any(|owned| owned.is_related_to(changed)))
            .cloned()
            .collect::<Vec<_>>();
        if !disallowed.is_empty() {
            return Err(PatchError::SemanticGuardViolation { paths: disallowed });
        }

        let mut changes = Vec::new();
        for path in &owned_paths {
            let before = PathState::from_semantic(value_at(&before_semantic, path));
            let after = PathState::from_semantic(value_at(&after_semantic, path));
            if before != after {
                changes.push(PathChange {
                    path: path.clone(),
                    before,
                    after,
                });
            }
        }

        // A semantically idempotent Set must be a byte-for-byte no-op too.
        // This avoids normalizing a user's spelling or whitespace for no reason.
        let replacement = if changes.is_empty() {
            snapshot.bytes.clone()
        } else {
            replacement
        };
        let requires_write = snapshot.materialize || !changes.is_empty();
        Ok(PreparedPatch {
            path: snapshot.path,
            format: snapshot.format,
            expected: snapshot.state,
            replacement,
            changes,
            changed_paths,
            requires_write,
            rollback: snapshot.rollback,
        })
    }

    pub fn prepare_file(
        path: impl Into<PathBuf>,
        format: ConfigFormat,
        operations: Vec<PatchOperation>,
    ) -> Result<PreparedPatch, PatchError> {
        Self::prepare(Self::snapshot(path, format)?, operations)
    }

    pub fn prepare_new_file_from_base(
        target: impl Into<PathBuf>,
        format: ConfigFormat,
        base: impl Into<Vec<u8>>,
        operations: Vec<PatchOperation>,
    ) -> Result<PreparedPatch, PatchError> {
        Self::prepare(
            ConfigSnapshot::for_missing_target_with_base(target, format, base)?,
            operations,
        )
    }

    pub fn commit(prepared: PreparedPatch) -> Result<AppliedPatch, PatchError> {
        if prepared.is_noop() {
            return Ok(AppliedPatch {
                path: prepared.path,
                format: prepared.format,
                before: prepared.expected,
                after: prepared.expected,
                changes: prepared.changes,
                wrote_file: false,
                rollback: prepared.rollback,
            });
        }

        let after_fingerprint = replace_atomically_if_unchanged(
            &prepared.path,
            &prepared.replacement,
            prepared.expected,
        )?;
        Ok(AppliedPatch {
            path: prepared.path,
            format: prepared.format,
            before: prepared.expected,
            after: ExpectedFileState::Present(after_fingerprint),
            changes: prepared.changes,
            wrote_file: true,
            rollback: prepared.rollback,
        })
    }

    pub fn apply_file(
        path: impl Into<PathBuf>,
        format: ConfigFormat,
        operations: Vec<PatchOperation>,
    ) -> Result<AppliedPatch, PatchError> {
        Self::commit(Self::prepare_file(path, format, operations)?)
    }

    pub fn apply_new_file_from_base(
        target: impl Into<PathBuf>,
        format: ConfigFormat,
        base: impl Into<Vec<u8>>,
        operations: Vec<PatchOperation>,
    ) -> Result<AppliedPatch, PatchError> {
        Self::commit(Self::prepare_new_file_from_base(
            target, format, base, operations,
        )?)
    }

    /// Restores the account-owned paths recorded by an applied patch.
    pub fn rollback(applied: &AppliedPatch) -> Result<RollbackOutcome, PatchError> {
        if let RollbackStrategy::WholeFile { previous } = &applied.rollback {
            return rollback_whole_file(applied, previous.as_deref());
        }
        if applied.changes.is_empty() {
            return Ok(RollbackOutcome::AlreadyRolledBack);
        }

        let current = match ConfigSnapshot::read(&applied.path, applied.format) {
            Ok(snapshot) => snapshot,
            Err(PatchError::Atomic(AtomicWriteError::Io { source, .. }))
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return if matches!(applied.before, ExpectedFileState::Missing) {
                    Ok(RollbackOutcome::AlreadyRolledBack)
                } else {
                    Ok(RollbackOutcome::Conflict {
                        paths: applied
                            .changes
                            .iter()
                            .map(|change| change.path.clone())
                            .collect(),
                    })
                };
            }
            Err(error) => return Err(error),
        };

        if !current.existed() {
            return if matches!(applied.before, ExpectedFileState::Missing) {
                Ok(RollbackOutcome::AlreadyRolledBack)
            } else {
                Ok(RollbackOutcome::Conflict {
                    paths: applied
                        .changes
                        .iter()
                        .map(|change| change.path.clone())
                        .collect(),
                })
            };
        }

        let mut reverse = Vec::new();
        for change in &applied.changes {
            reverse.push(operation_for_state(change.path.clone(), &change.before)?);
        }

        let prepared = Self::prepare(current, reverse)?;
        let receipt = Self::commit(prepared)?;
        let managed_paths = applied
            .changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        if matches!(applied.before, ExpectedFileState::Missing)
            && remove_created_file_if_empty(&applied.path, applied.format, &managed_paths)?
        {
            return Ok(RollbackOutcome::RemovedFileCreatedByPatch);
        }
        receipt
            .after_fingerprint()
            .map_or(Ok(RollbackOutcome::AlreadyRolledBack), |fingerprint| {
                Ok(RollbackOutcome::Restored { fingerprint })
            })
    }

    /// Replays a durable, field-level rollback receipt after a process restart.
    pub fn rollback_recorded(
        path: impl Into<PathBuf>,
        format: ConfigFormat,
        changes: Vec<PathChange>,
        config_existed: bool,
    ) -> Result<RollbackOutcome, PatchError> {
        let path = path.into();
        if changes.is_empty() {
            return Ok(RollbackOutcome::AlreadyRolledBack);
        }
        let current = ConfigSnapshot::read(&path, format)?;
        let mut reverse = Vec::new();
        for change in &changes {
            reverse.push(operation_for_state(change.path.clone(), &change.before)?);
        }
        if reverse.is_empty() {
            return Ok(RollbackOutcome::AlreadyRolledBack);
        }
        let receipt = Self::commit(Self::prepare(current, reverse)?)?;
        let managed_paths = changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        if !config_existed && remove_created_file_if_empty(&path, format, &managed_paths)? {
            return Ok(RollbackOutcome::RemovedFileCreatedByPatch);
        }
        receipt
            .after_fingerprint()
            .map_or(Ok(RollbackOutcome::AlreadyRolledBack), |fingerprint| {
                Ok(RollbackOutcome::Restored { fingerprint })
            })
    }
}

fn rollback_whole_file(
    applied: &AppliedPatch,
    previous: Option<&[u8]>,
) -> Result<RollbackOutcome, PatchError> {
    let ExpectedFileState::Present(_) = applied.after else {
        return Ok(RollbackOutcome::AlreadyRolledBack);
    };
    let observed = observed_state(&applied.path)?;
    if observed != applied.after {
        return Ok(RollbackOutcome::Conflict {
            paths: applied
                .changes
                .iter()
                .map(|change| change.path.clone())
                .collect(),
        });
    }
    match (applied.before, previous) {
        (ExpectedFileState::Missing, None) => {
            remove_atomically(&applied.path)?;
            Ok(RollbackOutcome::RemovedFileCreatedByPatch)
        }
        (ExpectedFileState::Present(before_fingerprint), Some(previous)) => {
            if FileFingerprint::from_bytes(previous) != before_fingerprint {
                return Err(PatchError::InvalidRollbackReceipt {
                    path: applied.path.clone(),
                });
            }
            let restored = replace_atomically(&applied.path, previous)?;
            Ok(RollbackOutcome::Restored {
                fingerprint: restored,
            })
        }
        _ => Err(PatchError::InvalidRollbackReceipt {
            path: applied.path.clone(),
        }),
    }
}

fn remove_created_file_if_empty(
    path: &Path,
    format: ConfigFormat,
    managed_paths: &[OwnedPath],
) -> Result<bool, PatchError> {
    let Some(bytes) = read_file(path)? else {
        return Ok(false);
    };
    if !contains_only_managed_empty_parents(
        &parse_semantic(&bytes, format, true)?,
        managed_paths,
        &mut Vec::new(),
    ) || contains_user_comment(&bytes, format)
    {
        return Ok(false);
    }
    remove_atomically(path)?;
    Ok(true)
}

fn contains_user_comment(bytes: &[u8], format: ConfigFormat) -> bool {
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let source = String::from_utf8_lossy(body);
    match format {
        ConfigFormat::Toml => source.contains('#'),
        ConfigFormat::Json => false,
        ConfigFormat::Jsonc => source.contains("//") || source.contains("/*"),
    }
}

fn contains_only_managed_empty_parents(
    value: &SemanticValue,
    managed_paths: &[OwnedPath],
    prefix: &mut Vec<String>,
) -> bool {
    match value {
        SemanticValue::Object(values) => values.iter().all(|(key, value)| {
            prefix.push(key.clone());
            let is_managed_parent = managed_paths.iter().any(|path| {
                path.segments().len() > prefix.len()
                    && path.segments()[..prefix.len()] == prefix[..]
            });
            let empty = is_managed_parent
                && contains_only_managed_empty_parents(value, managed_paths, prefix);
            prefix.pop();
            empty
        }),
        _ => false,
    }
}

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("config paths must be absolute: {path}")]
    RelativeConfigPath { path: PathBuf },

    #[error("invalid owned path {path:?}: {reason}")]
    InvalidOwnedPath { path: String, reason: &'static str },

    #[error("owned paths overlap ({first} and {second}); use one unambiguous operation")]
    OverlappingOwnedPaths { first: OwnedPath, second: OwnedPath },

    #[error("config is too large ({bytes} bytes, maximum {maximum}): {path}")]
    ConfigTooLarge {
        path: PathBuf,
        bytes: usize,
        maximum: usize,
    },

    #[error("config is not valid UTF-8: {path}")]
    NonUtf8 { path: PathBuf },

    #[error("could not parse {format}: {message}")]
    Parse {
        format: &'static str,
        message: String,
    },

    #[error("ambiguous duplicate JSON object key at {path}")]
    DuplicateJsonKey { path: OwnedPath },

    #[error("owned path {path} crosses a non-object {found}")]
    PathTypeConflict {
        path: OwnedPath,
        found: &'static str,
    },

    #[error("value at {path} cannot be represented in {format}: {reason}")]
    UnsupportedValue {
        path: OwnedPath,
        format: &'static str,
        reason: &'static str,
    },

    #[error("renderer changed non-owned semantic paths: {paths:?}")]
    SemanticGuardViolation { paths: Vec<OwnedPath> },

    #[error("patch rollback receipt is internally inconsistent for {path}")]
    InvalidRollbackReceipt { path: PathBuf },

    #[error(transparent)]
    Atomic(#[from] AtomicWriteError),
}

impl PatchError {
    pub fn is_external_conflict(&self) -> bool {
        matches!(self, Self::Atomic(AtomicWriteError::Conflict { .. }))
    }
}

#[derive(Clone, Debug, PartialEq)]
enum SemanticValue {
    Null,
    Bool(bool),
    Integer(i64),
    Number(String),
    String(String),
    Datetime(String),
    Array(Vec<SemanticValue>),
    Object(BTreeMap<String, SemanticValue>),
}

impl SemanticValue {
    fn empty_object() -> Self {
        Self::Object(BTreeMap::new())
    }

    fn to_public_json(&self) -> JsonValueOwned {
        match self {
            Self::Null => JsonValueOwned::Null,
            Self::Bool(value) => JsonValueOwned::Bool(*value),
            Self::Integer(value) => JsonValueOwned::Number((*value).into()),
            Self::Number(value) => serde_json::Number::from_str(value).map_or_else(
                |_| JsonValueOwned::String(value.clone()),
                JsonValueOwned::Number,
            ),
            Self::String(value) | Self::Datetime(value) => JsonValueOwned::String(value.clone()),
            Self::Array(values) => {
                JsonValueOwned::Array(values.iter().map(SemanticValue::to_public_json).collect())
            }
            Self::Object(values) => JsonValueOwned::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_public_json()))
                    .collect(),
            ),
        }
    }
}

fn validate_path_segments(segments: &[String]) -> Result<(), PatchError> {
    let printable = || {
        let path = OwnedPath(segments.to_vec());
        path.to_json_pointer()
    };
    if segments.is_empty() {
        return Err(PatchError::InvalidOwnedPath {
            path: String::new(),
            reason: "the document root cannot be owned",
        });
    }
    if segments.len() > MAX_PATH_SEGMENTS {
        return Err(PatchError::InvalidOwnedPath {
            path: printable(),
            reason: "too many path segments",
        });
    }
    if segments.iter().map(String::len).sum::<usize>() > MAX_PATH_BYTES {
        return Err(PatchError::InvalidOwnedPath {
            path: printable(),
            reason: "path is too long",
        });
    }
    if segments.iter().any(|segment| segment.contains('\0')) {
        return Err(PatchError::InvalidOwnedPath {
            path: printable(),
            reason: "path keys cannot contain NUL",
        });
    }
    Ok(())
}

fn validate_operations(operations: &[PatchOperation]) -> Result<(), PatchError> {
    for (index, operation) in operations.iter().enumerate() {
        for other in operations.iter().skip(index + 1) {
            if operation.path().is_related_to(other.path()) {
                return Err(PatchError::OverlappingOwnedPaths {
                    first: operation.path().clone(),
                    second: other.path().clone(),
                });
            }
        }
    }
    Ok(())
}

fn render_patch(
    snapshot: &ConfigSnapshot,
    operations: &[PatchOperation],
) -> Result<Vec<u8>, PatchError> {
    let (had_bom, source) = decode_utf8(&snapshot.bytes, &snapshot.path)?;
    let rendered = match snapshot.format {
        ConfigFormat::Toml => render_toml(source, operations)?,
        ConfigFormat::Json | ConfigFormat::Jsonc => {
            render_jsonc(source, snapshot.format, operations)?
        }
    };
    let rendered = preserve_consistent_crlf(source, rendered);
    let mut bytes = Vec::with_capacity(rendered.len() + usize::from(had_bom) * UTF8_BOM.len());
    if had_bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(rendered.as_bytes());
    Ok(bytes)
}

fn render_jsonc(
    source: &str,
    format: ConfigFormat,
    operations: &[PatchOperation],
) -> Result<String, PatchError> {
    if operations.is_empty() {
        return Ok(source.to_owned());
    }
    let options = json_parse_options(format);
    let root = CstRootNode::parse(source, &options).map_err(|error| PatchError::Parse {
        format: format.name(),
        message: error.to_string(),
    })?;
    validate_json_duplicates(&root)?;
    let object = root
        .object_value_or_create()
        .ok_or_else(|| PatchError::PathTypeConflict {
            path: operations[0].path().clone(),
            found: "document root",
        })?;
    for operation in operations {
        match operation {
            PatchOperation::Set { path, value } => {
                json_set(&object, path, value)?;
            }
            PatchOperation::Remove { path } => {
                json_remove(&object, path)?;
            }
        }
    }
    Ok(root.to_string())
}

fn json_set(root: &CstObject, path: &OwnedPath, value: &JsonValueOwned) -> Result<(), PatchError> {
    let mut object = root.clone();
    for (index, segment) in path.segments().iter().enumerate() {
        let is_leaf = index + 1 == path.segments().len();
        if is_leaf {
            let input = json_to_cst(value);
            if let Some(property) = object.get(segment) {
                property.set_value(input);
            } else {
                object.append(segment, input);
            }
            return Ok(());
        }
        object = match object.get(segment) {
            Some(property) => {
                property
                    .object_value()
                    .ok_or_else(|| PatchError::PathTypeConflict {
                        path: prefix_path(path, index + 1),
                        found: "JSON value",
                    })?
            }
            None => object.object_value_or_create(segment).ok_or_else(|| {
                PatchError::PathTypeConflict {
                    path: prefix_path(path, index + 1),
                    found: "JSON value",
                }
            })?,
        };
    }
    Ok(())
}

fn json_remove(root: &CstObject, path: &OwnedPath) -> Result<(), PatchError> {
    let mut object = root.clone();
    for (index, segment) in path.segments().iter().enumerate() {
        let is_leaf = index + 1 == path.segments().len();
        let Some(property) = object.get(segment) else {
            return Ok(());
        };
        if is_leaf {
            property.remove();
            return Ok(());
        }
        object = property
            .object_value()
            .ok_or_else(|| PatchError::PathTypeConflict {
                path: prefix_path(path, index + 1),
                found: "JSON value",
            })?;
    }
    Ok(())
}

fn json_to_cst(value: &JsonValueOwned) -> CstInputValue {
    match value {
        JsonValueOwned::Null => CstInputValue::Null,
        JsonValueOwned::Bool(value) => CstInputValue::Bool(*value),
        JsonValueOwned::Number(value) => CstInputValue::Number(value.to_string()),
        JsonValueOwned::String(value) => CstInputValue::String(value.clone()),
        JsonValueOwned::Array(values) => {
            CstInputValue::Array(values.iter().map(json_to_cst).collect())
        }
        JsonValueOwned::Object(values) => CstInputValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), json_to_cst(value)))
                .collect(),
        ),
    }
}

fn validate_json_duplicates(root: &CstRootNode) -> Result<(), PatchError> {
    let Some(value) = root.value() else {
        return Ok(());
    };
    validate_json_node_duplicates(&value, &[])
}

fn validate_json_node_duplicates(node: &CstNode, path: &[String]) -> Result<(), PatchError> {
    if let Some(object) = node.as_object() {
        let mut names = BTreeSet::new();
        for property in object.properties() {
            let name = property
                .name()
                .ok_or_else(|| PatchError::Parse {
                    format: "JSON/JSONC",
                    message: "object property is missing a name".to_owned(),
                })?
                .decoded_value()
                .map_err(|error| PatchError::Parse {
                    format: "JSON/JSONC",
                    message: format!("invalid object property name: {error:?}"),
                })?;
            let mut child_path = path.to_vec();
            child_path.push(name.clone());
            if !names.insert(name) {
                return Err(PatchError::DuplicateJsonKey {
                    path: OwnedPath(child_path),
                });
            }
            if let Some(value) = property.value() {
                validate_json_node_duplicates(&value, &child_path)?;
            }
        }
    } else if let Some(array) = node.as_array() {
        for element in array.elements() {
            validate_json_node_duplicates(&element, path)?;
        }
    }
    Ok(())
}

fn render_toml(source: &str, operations: &[PatchOperation]) -> Result<String, PatchError> {
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| PatchError::Parse {
            format: ConfigFormat::Toml.name(),
            message: error.to_string(),
        })?;
    for operation in operations {
        match operation {
            PatchOperation::Set { path, value } => {
                let value = json_to_toml_item(value, path)?;
                promote_inline_toml_parents(document.as_item_mut(), path, 0)?;
                toml_set_item(document.as_item_mut(), path, value, 0)?;
            }
            PatchOperation::Remove { path } => {
                toml_remove_item(document.as_item_mut(), path, 0)?;
            }
        }
    }
    Ok(document.to_string())
}

fn promote_inline_toml_parents(
    item: &mut Item,
    path: &OwnedPath,
    depth: usize,
) -> Result<(), PatchError> {
    if depth + 1 >= path.segments().len() {
        return Ok(());
    }
    let Item::Table(table) = item else {
        return Ok(());
    };
    let segment = &path.segments()[depth];
    let Some(child) = table.get_mut(segment) else {
        return Ok(());
    };
    if matches!(child, Item::Value(TomlValue::InlineTable(_))) {
        let Item::Value(TomlValue::InlineTable(inline)) = std::mem::replace(child, Item::None)
        else {
            unreachable!();
        };
        let mut promoted = Table::new();
        for (key, value) in inline {
            promoted.insert(&key, Item::Value(value));
        }
        *child = Item::Table(promoted);
    }
    promote_inline_toml_parents(child, path, depth + 1)
}

fn toml_set_item(
    item: &mut Item,
    path: &OwnedPath,
    mut new_value: Item,
    depth: usize,
) -> Result<(), PatchError> {
    let segment = &path.segments()[depth];
    let is_leaf = depth + 1 == path.segments().len();
    match item {
        Item::Table(table) => {
            if is_leaf {
                match table.get_mut(segment) {
                    Some(existing) => {
                        if let Item::Value(old_value) = &mut *existing
                            && let Item::Value(replacement) = &mut new_value
                        {
                            preserve_toml_decor(old_value, replacement);
                        }
                        *existing = new_value;
                    }
                    None => {
                        table.insert(segment, new_value);
                    }
                }
                return Ok(());
            }

            if table.get(segment).is_none() {
                let mut child = Table::new();
                child.set_implicit(true);
                table.insert(segment, Item::Table(child));
            }
            let child = table
                .get_mut(segment)
                .expect("the missing TOML parent was inserted");
            toml_set_item(child, path, new_value, depth + 1)
        }
        Item::Value(TomlValue::InlineTable(table)) => {
            let value = item_to_inline_value(new_value, path)?;
            toml_set_inline(table, path, value, depth)
        }
        existing => Err(PatchError::PathTypeConflict {
            path: prefix_path(path, depth),
            found: existing.type_name(),
        }),
    }
}

fn toml_set_inline(
    table: &mut InlineTable,
    path: &OwnedPath,
    new_value: TomlValue,
    depth: usize,
) -> Result<(), PatchError> {
    let segment = &path.segments()[depth];
    let is_leaf = depth + 1 == path.segments().len();
    if is_leaf {
        if let Some(old_value) = table.get_mut(segment) {
            let mut replacement = new_value;
            preserve_toml_decor(old_value, &mut replacement);
            *old_value = replacement;
        } else {
            table.insert(segment, new_value);
        }
        return Ok(());
    }
    if table.get(segment).is_none() {
        table.insert(segment, TomlValue::InlineTable(InlineTable::new()));
    }
    let child = table
        .get_mut(segment)
        .expect("the missing inline TOML parent was inserted");
    match child {
        TomlValue::InlineTable(child) => toml_set_inline(child, path, new_value, depth + 1),
        other => Err(PatchError::PathTypeConflict {
            path: prefix_path(path, depth + 1),
            found: other.type_name(),
        }),
    }
}

fn toml_remove_item(item: &mut Item, path: &OwnedPath, depth: usize) -> Result<(), PatchError> {
    let segment = &path.segments()[depth];
    let is_leaf = depth + 1 == path.segments().len();
    match item {
        Item::Table(table) => {
            if is_leaf {
                table.remove(segment);
                return Ok(());
            }
            let Some(child) = table.get_mut(segment) else {
                return Ok(());
            };
            toml_remove_item(child, path, depth + 1)
        }
        Item::Value(TomlValue::InlineTable(table)) => toml_remove_inline(table, path, depth),
        existing => Err(PatchError::PathTypeConflict {
            path: prefix_path(path, depth),
            found: existing.type_name(),
        }),
    }
}

fn toml_remove_inline(
    table: &mut InlineTable,
    path: &OwnedPath,
    depth: usize,
) -> Result<(), PatchError> {
    let segment = &path.segments()[depth];
    let is_leaf = depth + 1 == path.segments().len();
    if is_leaf {
        table.remove(segment);
        return Ok(());
    }
    let Some(child) = table.get_mut(segment) else {
        return Ok(());
    };
    match child {
        TomlValue::InlineTable(child) => toml_remove_inline(child, path, depth + 1),
        other => Err(PatchError::PathTypeConflict {
            path: prefix_path(path, depth + 1),
            found: other.type_name(),
        }),
    }
}

fn preserve_toml_decor(old: &TomlValue, replacement: &mut TomlValue) {
    *replacement.decor_mut() = old.decor().clone();
}

fn json_to_toml(value: &JsonValueOwned, path: &OwnedPath) -> Result<TomlValue, PatchError> {
    match value {
        JsonValueOwned::Null => Err(PatchError::UnsupportedValue {
            path: path.clone(),
            format: ConfigFormat::Toml.name(),
            reason: "TOML has no null value; remove the path instead",
        }),
        JsonValueOwned::Bool(value) => Ok(TomlValue::from(*value)),
        JsonValueOwned::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(TomlValue::from(value))
            } else if let Some(value) = value.as_u64() {
                i64::try_from(value).map(TomlValue::from).map_err(|_| {
                    PatchError::UnsupportedValue {
                        path: path.clone(),
                        format: ConfigFormat::Toml.name(),
                        reason: "integer is outside TOML's signed 64-bit range",
                    }
                })
            } else if let Some(value) = value.as_f64() {
                Ok(TomlValue::from(value))
            } else {
                Err(PatchError::UnsupportedValue {
                    path: path.clone(),
                    format: ConfigFormat::Toml.name(),
                    reason: "unsupported numeric value",
                })
            }
        }
        JsonValueOwned::String(value) => Ok(TomlValue::from(value.clone())),
        JsonValueOwned::Array(values) => {
            let mut array = Array::new();
            for value in values {
                array.push(json_to_toml(value, path)?);
            }
            Ok(TomlValue::Array(array))
        }
        JsonValueOwned::Object(values) => {
            let mut table = InlineTable::new();
            for (key, value) in values {
                table.insert(key, json_to_toml(value, path)?);
            }
            Ok(TomlValue::InlineTable(table))
        }
    }
}

fn json_to_toml_item(value: &JsonValueOwned, path: &OwnedPath) -> Result<Item, PatchError> {
    match value {
        JsonValueOwned::Object(values) => {
            let mut table = Table::new();
            for (key, value) in values {
                table.insert(key, json_to_toml_item(value, path)?);
            }
            Ok(Item::Table(table))
        }
        _ => json_to_toml(value, path).map(Item::Value),
    }
}

fn item_to_inline_value(item: Item, path: &OwnedPath) -> Result<TomlValue, PatchError> {
    match item {
        Item::Value(value) => Ok(value),
        Item::Table(table) => {
            let mut inline = InlineTable::new();
            for (key, item) in table {
                inline.insert(&key, item_to_inline_value(item, path)?);
            }
            Ok(TomlValue::InlineTable(inline))
        }
        item => Err(PatchError::UnsupportedValue {
            path: path.clone(),
            format: ConfigFormat::Toml.name(),
            reason: item.type_name(),
        }),
    }
}

fn parse_semantic(
    bytes: &[u8],
    format: ConfigFormat,
    existed: bool,
) -> Result<SemanticValue, PatchError> {
    let (_, source) = decode_utf8(bytes, Path::new("<in-memory config>"))?;
    match format {
        ConfigFormat::Toml => {
            let document = source
                .parse::<DocumentMut>()
                .map_err(|error| PatchError::Parse {
                    format: format.name(),
                    message: error.to_string(),
                })?;
            Ok(toml_item_to_semantic(document.as_item()))
        }
        ConfigFormat::Json | ConfigFormat::Jsonc => {
            let parsed = parse_to_value(source, &json_parse_options(format)).map_err(|error| {
                PatchError::Parse {
                    format: format.name(),
                    message: error.to_string(),
                }
            })?;
            match parsed {
                Some(value) => Ok(json_value_to_semantic(value)?),
                None if !existed || source.trim().is_empty() => Ok(SemanticValue::empty_object()),
                None => Err(PatchError::Parse {
                    format: format.name(),
                    message: "document has no root value".to_owned(),
                }),
            }
        }
    }
}

fn json_parse_options(format: ConfigFormat) -> ParseOptions {
    let is_jsonc = format == ConfigFormat::Jsonc;
    ParseOptions {
        allow_comments: is_jsonc,
        allow_loose_object_property_names: false,
        allow_trailing_commas: is_jsonc,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn json_value_to_semantic(value: JsonValue<'_>) -> Result<SemanticValue, PatchError> {
    match value {
        JsonValue::String(value) => Ok(SemanticValue::String(value.into_owned())),
        JsonValue::Number(value) => {
            let number =
                serde_json::Number::from_str(value).map_err(|error| PatchError::Parse {
                    format: "JSON/JSONC",
                    message: error.to_string(),
                })?;
            Ok(SemanticValue::Number(number.to_string()))
        }
        JsonValue::Boolean(value) => Ok(SemanticValue::Bool(value)),
        JsonValue::Null => Ok(SemanticValue::Null),
        JsonValue::Array(values) => values
            .into_iter()
            .map(json_value_to_semantic)
            .collect::<Result<Vec<_>, _>>()
            .map(SemanticValue::Array),
        JsonValue::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key.into_owned(), json_value_to_semantic(value)?)))
            .collect::<Result<BTreeMap<_, _>, PatchError>>()
            .map(SemanticValue::Object),
    }
}

fn toml_item_to_semantic(item: &Item) -> SemanticValue {
    match item {
        Item::None => SemanticValue::Null,
        Item::Value(value) => toml_value_to_semantic(value),
        Item::Table(table) => SemanticValue::Object(
            table
                .iter()
                .map(|(key, value)| (key.to_owned(), toml_item_to_semantic(value)))
                .collect(),
        ),
        Item::ArrayOfTables(tables) => SemanticValue::Array(
            tables
                .iter()
                .map(|table| {
                    SemanticValue::Object(
                        table
                            .iter()
                            .map(|(key, value)| (key.to_owned(), toml_item_to_semantic(value)))
                            .collect(),
                    )
                })
                .collect(),
        ),
    }
}

fn toml_value_to_semantic(value: &TomlValue) -> SemanticValue {
    match value {
        TomlValue::String(value) => SemanticValue::String(value.value().clone()),
        TomlValue::Integer(value) => SemanticValue::Integer(*value.value()),
        TomlValue::Float(value) => SemanticValue::Number(value.value().to_string()),
        TomlValue::Boolean(value) => SemanticValue::Bool(*value.value()),
        TomlValue::Datetime(value) => SemanticValue::Datetime(value.value().to_string()),
        TomlValue::Array(values) => {
            SemanticValue::Array(values.iter().map(toml_value_to_semantic).collect())
        }
        TomlValue::InlineTable(values) => SemanticValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.to_owned(), toml_value_to_semantic(value)))
                .collect(),
        ),
    }
}

fn semantic_diff(before: &SemanticValue, after: &SemanticValue) -> Vec<OwnedPath> {
    let mut changes = Vec::new();
    collect_semantic_diff(Some(before), Some(after), &mut Vec::new(), &mut changes);
    changes
}

fn collect_semantic_diff(
    before: Option<&SemanticValue>,
    after: Option<&SemanticValue>,
    path: &mut Vec<String>,
    changes: &mut Vec<OwnedPath>,
) {
    match (before, after) {
        (Some(SemanticValue::Object(before)), Some(SemanticValue::Object(after))) => {
            let keys = before.keys().chain(after.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                path.push(key.clone());
                collect_semantic_diff(before.get(key), after.get(key), path, changes);
                path.pop();
            }
        }
        (None, Some(SemanticValue::Object(values)))
        | (Some(SemanticValue::Object(values)), None)
            if !values.is_empty() =>
        {
            for (key, value) in values {
                path.push(key.clone());
                match before {
                    None => collect_semantic_diff(None, Some(value), path, changes),
                    Some(_) => collect_semantic_diff(Some(value), None, path, changes),
                }
                path.pop();
            }
        }
        (before, after) if before == after => {}
        _ if !path.is_empty() => changes.push(OwnedPath(path.clone())),
        _ => {
            // A root type change is never an owned-path edit. Represent it by a
            // synthetic path that cannot match a valid adapter whitelist.
            changes.push(OwnedPath(vec!["<document-root>".to_owned()]));
        }
    }
}

fn value_at<'a>(root: &'a SemanticValue, path: &OwnedPath) -> Option<&'a SemanticValue> {
    let mut current = root;
    for segment in path.segments() {
        let SemanticValue::Object(object) = current else {
            return None;
        };
        current = object.get(segment)?;
    }
    Some(current)
}

fn operation_for_state(path: OwnedPath, state: &PathState) -> Result<PatchOperation, PatchError> {
    if !state.exists {
        return Ok(PatchOperation::Remove { path });
    }
    let value = state
        .value
        .clone()
        .ok_or_else(|| PatchError::UnsupportedValue {
            path: path.clone(),
            format: "config",
            reason: "an existing path state must contain a value",
        })?;
    Ok(PatchOperation::Set { path, value })
}

fn prefix_path(path: &OwnedPath, length: usize) -> OwnedPath {
    OwnedPath(path.segments()[..length.max(1)].to_vec())
}

fn decode_utf8<'a>(bytes: &'a [u8], path: &Path) -> Result<(bool, &'a str), PatchError> {
    let (had_bom, body) = if bytes.starts_with(UTF8_BOM) {
        (true, &bytes[UTF8_BOM.len()..])
    } else {
        (false, bytes)
    };
    let source = std::str::from_utf8(body).map_err(|_| PatchError::NonUtf8 {
        path: path.to_path_buf(),
    })?;
    Ok((had_bom, source))
}

fn preserve_consistent_crlf(source: &str, rendered: String) -> String {
    if source.contains("\r\n") && !contains_bare_lf(source) {
        let mut normalized = String::with_capacity(rendered.len());
        let mut previous = '\0';
        for character in rendered.chars() {
            if character == '\n' && previous != '\r' {
                normalized.push('\r');
            }
            normalized.push(character);
            previous = character;
        }
        normalized
    } else {
        rendered
    }
}

fn contains_bare_lf(text: &str) -> bool {
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            return true;
        }
        previous = character;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn path(segments: &[&str]) -> OwnedPath {
        OwnedPath::from_segments(segments.iter().copied()).unwrap()
    }

    #[test]
    fn jsonc_patch_preserves_comments_order_bom_crlf_and_unknown_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.jsonc");
        let before = b"\xef\xbb\xbf{\r\n  // keep this\r\n  \"theme\": \"dark\",\r\n  \"env\": {\r\n    \"TOKEN\": \"old\", // account field\r\n    \"UNOWNED\": \"same\"\r\n  },\r\n  \"tail\": [1, 2, 3]\r\n}\r\n";
        fs::write(&config_path, before).unwrap();

        PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Jsonc,
            vec![PatchOperation::set(
                path(&["env", "TOKEN"]),
                JsonValueOwned::String("new".to_owned()),
            )],
        )
        .unwrap();

        let after = fs::read(&config_path).unwrap();
        assert!(after.starts_with(UTF8_BOM));
        let after = std::str::from_utf8(&after[UTF8_BOM.len()..]).unwrap();
        assert!(after.contains("// keep this\r\n"));
        assert!(after.contains("\"UNOWNED\": \"same\"\r\n"));
        assert!(after.contains("\"tail\": [1, 2, 3]\r\n"));
        assert!(after.contains("\"TOKEN\": \"new\", // account field\r\n"));
        assert!(!contains_bare_lf(after));
    }

    #[test]
    fn toml_patch_only_changes_owned_value_and_preserves_comment() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let before = "# global comment\r\ntheme = 'dark'\r\nmodel_provider = \"old\" # owned comment\r\n\r\n[unowned]\r\ncustom = { x = 1, y = 2 }\r\n";
        fs::write(&config_path, before).unwrap();

        PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Toml,
            vec![PatchOperation::set(
                path(&["model_provider"]),
                JsonValueOwned::String("new".to_owned()),
            )],
        )
        .unwrap();

        let after = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            after,
            before.replace("model_provider = \"old\"", "model_provider = \"new\"")
        );
    }

    #[test]
    fn semantic_noop_is_byte_for_byte_noop() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let before = b"model_provider='same' # unusual spelling\n";
        fs::write(&config_path, before).unwrap();

        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Toml,
            vec![PatchOperation::set(
                path(&["model_provider"]),
                JsonValueOwned::String("same".to_owned()),
            )],
        )
        .unwrap();

        assert!(!receipt.wrote_file());
        assert_eq!(fs::read(config_path).unwrap(), before);
    }

    #[test]
    fn rollback_preserves_unrelated_edits_made_after_activation() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.jsonc");
        fs::write(
            &config_path,
            "{\n  \"env\": { \"TOKEN\": \"old\" },\n  \"theme\": \"dark\"\n}\n",
        )
        .unwrap();
        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Jsonc,
            vec![PatchOperation::set(
                path(&["env", "TOKEN"]),
                JsonValueOwned::String("new".to_owned()),
            )],
        )
        .unwrap();
        let activated = fs::read_to_string(&config_path).unwrap();
        fs::write(&config_path, activated.replace("\"dark\"", "\"light\"")).unwrap();

        let outcome = PatchEngine::rollback(&receipt).unwrap();

        assert!(matches!(outcome, RollbackOutcome::Restored { .. }));
        let after = fs::read_to_string(config_path).unwrap();
        assert!(after.contains("\"TOKEN\": \"old\""));
        assert!(after.contains("\"theme\": \"light\""));
    }

    #[test]
    fn missing_target_can_be_materialized_from_an_unchanged_base_and_rolled_back() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("managed").join("settings.jsonc");
        fs::create_dir(config_path.parent().unwrap()).unwrap();
        let base = b"{\r\n  // copied verbatim\r\n  \"theme\": \"dark\"\r\n}\r\n".to_vec();

        let receipt = PatchEngine::apply_new_file_from_base(
            &config_path,
            ConfigFormat::Jsonc,
            base.clone(),
            Vec::new(),
        )
        .unwrap();

        assert!(receipt.wrote_file());
        assert_eq!(fs::read(&config_path).unwrap(), base);
        assert_eq!(
            PatchEngine::rollback(&receipt).unwrap(),
            RollbackOutcome::RemovedFileCreatedByPatch
        );
        assert!(!config_path.exists());
    }

    #[test]
    fn duplicate_json_keys_are_rejected_as_ambiguous() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.jsonc");
        fs::write(&config_path, r#"{"env": {}, "env": {}}"#).unwrap();

        let error = PatchEngine::prepare_file(
            &config_path,
            ConfigFormat::Jsonc,
            vec![PatchOperation::set(
                path(&["env", "TOKEN"]),
                JsonValueOwned::String("new".to_owned()),
            )],
        )
        .unwrap_err();

        assert!(matches!(error, PatchError::DuplicateJsonKey { .. }));
    }

    #[test]
    fn overlapping_whitelist_paths_are_rejected() {
        let operations = vec![
            PatchOperation::remove(path(&["env"])),
            PatchOperation::remove(path(&["env", "TOKEN"])),
        ];

        let error = validate_operations(&operations).unwrap_err();

        assert!(matches!(error, PatchError::OverlappingOwnedPaths { .. }));
    }

    #[test]
    fn durable_rollback_restores_only_recorded_paths_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.json");
        fs::write(
            &config_path,
            r#"{"account":"old","theme":"dark","external":"before"}"#,
        )
        .unwrap();
        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Json,
            vec![PatchOperation::set(
                path(&["account"]),
                JsonValueOwned::String("new".into()),
            )],
        )
        .unwrap();
        let changes = receipt.changes().to_vec();

        // Simulate an unrelated edit made after YAAT exited unexpectedly.
        let after = fs::read_to_string(&config_path).unwrap();
        fs::write(
            &config_path,
            after.replace(r#""external":"before""#, r#""external":"after""#),
        )
        .unwrap();

        assert!(matches!(
            PatchEngine::rollback_recorded(&config_path, ConfigFormat::Json, changes, true)
                .unwrap(),
            RollbackOutcome::Restored { .. }
        ));
        let restored: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(restored["account"], "old");
        assert_eq!(restored["theme"], "dark");
        assert_eq!(restored["external"], "after");
    }

    #[test]
    fn commit_rejects_an_unrelated_edit_after_prepare() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.json");
        fs::write(&config_path, r#"{"account":"old","theme":"dark"}"#).unwrap();
        let prepared = PatchEngine::prepare_file(
            &config_path,
            ConfigFormat::Json,
            vec![PatchOperation::set(
                path(&["account"]),
                JsonValueOwned::String("new".into()),
            )],
        )
        .unwrap();
        fs::write(&config_path, r#"{"account":"old","theme":"light"}"#).unwrap();

        let error = PatchEngine::commit(prepared).unwrap_err();

        assert!(error.is_external_conflict());
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            r#"{"account":"old","theme":"light"}"#
        );
    }

    #[test]
    fn durable_rollback_removes_a_file_created_by_the_patch() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.json");
        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Json,
            vec![PatchOperation::set(
                path(&["account"]),
                JsonValueOwned::String("new".into()),
            )],
        )
        .unwrap();

        let outcome = PatchEngine::rollback_recorded(
            &config_path,
            ConfigFormat::Json,
            receipt.changes().to_vec(),
            false,
        )
        .unwrap();

        assert_eq!(outcome, RollbackOutcome::RemovedFileCreatedByPatch);
        assert!(!config_path.exists());
    }

    #[test]
    fn durable_rollback_keeps_unrelated_content_added_to_a_created_file() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.json");
        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Json,
            vec![PatchOperation::set(
                path(&["account"]),
                JsonValueOwned::String("new".into()),
            )],
        )
        .unwrap();
        fs::write(&config_path, r#"{"account":"new","theme":"light"}"#).unwrap();

        PatchEngine::rollback_recorded(
            &config_path,
            ConfigFormat::Json,
            receipt.changes().to_vec(),
            false,
        )
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(config_path).unwrap())
                .unwrap(),
            serde_json::json!({ "theme": "light" })
        );
    }

    #[test]
    fn durable_rollback_keeps_comments_added_to_a_created_jsonc_file() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.jsonc");
        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Jsonc,
            vec![PatchOperation::set(
                path(&["account"]),
                JsonValueOwned::String("new".into()),
            )],
        )
        .unwrap();
        let edited = fs::read_to_string(&config_path).unwrap().replacen(
            '{',
            "{\n  // keep this user note\n",
            1,
        );
        fs::write(&config_path, edited).unwrap();

        PatchEngine::rollback_recorded(
            &config_path,
            ConfigFormat::Jsonc,
            receipt.changes().to_vec(),
            false,
        )
        .unwrap();

        assert!(config_path.exists());
        assert!(
            fs::read_to_string(config_path)
                .unwrap()
                .contains("keep this user note")
        );
    }

    #[test]
    fn durable_rollback_keeps_an_unrelated_empty_object() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("settings.json");
        let receipt = PatchEngine::apply_file(
            &config_path,
            ConfigFormat::Json,
            vec![PatchOperation::set(
                path(&["account", "token"]),
                JsonValueOwned::String("new".into()),
            )],
        )
        .unwrap();
        fs::write(&config_path, r#"{"account":{"token":"new"},"custom":{}}"#).unwrap();

        PatchEngine::rollback_recorded(
            &config_path,
            ConfigFormat::Json,
            receipt.changes().to_vec(),
            false,
        )
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(config_path).unwrap())
                .unwrap(),
            serde_json::json!({ "account": {}, "custom": {} })
        );
    }
}
