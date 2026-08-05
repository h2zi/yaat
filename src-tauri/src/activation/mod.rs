//! Safe activation primitives.
//!
//! Platform adapters describe exactly which logical config paths they own and
//! submit those paths to [`PatchEngine`]. The engine performs a semantic
//! whitelist check, durable atomic replacement, and field-level rollback.
//! Adapters must never serialize an entire user config from a profile model.

mod atomic;
mod patch;

pub use atomic::{AtomicWriteError, ExpectedFileState, FileFingerprint};
pub(crate) use atomic::{remove_atomically, replace_atomically};
pub use patch::{
    AppliedPatch, ConfigFormat, ConfigSnapshot, OwnedPath, PatchEngine, PatchError, PatchOperation,
    PathChange, PathState, PreparedPatch, RollbackOutcome,
};
