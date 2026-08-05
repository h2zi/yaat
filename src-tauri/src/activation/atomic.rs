//! Same-directory atomic file replacement and file-state fingerprints.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const MAX_TEMP_ATTEMPTS: usize = 32;

/// SHA-256 fingerprint used for compare-and-swap file publication.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileFingerprint([u8; 32]);

impl FileFingerprint {
    /// Hashes an in-memory file image.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Returns the raw SHA-256 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encodes the fingerprint as lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for FileFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FileFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for FileFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// Expected state used by compare-and-swap file operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpectedFileState {
    Missing,
    Present(FileFingerprint),
}

impl ExpectedFileState {
    /// Returns the fingerprint when the file is expected to exist.
    #[must_use]
    pub const fn fingerprint(self) -> Option<FileFingerprint> {
        match self {
            Self::Missing => None,
            Self::Present(fingerprint) => Some(fingerprint),
        }
    }
}

/// Failure while inspecting, publishing, or removing a managed file.
#[derive(Debug, Error)]
pub enum AtomicWriteError {
    #[error("the config path has no parent: {path}")]
    MissingParent { path: PathBuf },

    #[error("refusing to replace non-regular config path: {path}")]
    NonRegularFile { path: PathBuf },

    #[error("config changed outside YAAT (expected {expected:?}, observed {observed:?})")]
    Conflict {
        expected: ExpectedFileState,
        observed: ExpectedFileState,
    },

    #[error("could not allocate a same-directory temporary file for {path}")]
    TempNameExhausted { path: PathBuf },

    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(crate) fn read_file(path: &Path) -> Result<Option<Vec<u8>>, AtomicWriteError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("inspect config", path, source)),
    };
    validate_regular_file(path, &metadata)?;

    let mut file = File::open(path).map_err(|source| io_error("open config", path, source))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| io_error("read config", path, source))?;
    Ok(Some(bytes))
}

pub(crate) fn observed_state(path: &Path) -> Result<ExpectedFileState, AtomicWriteError> {
    Ok(
        read_file(path)?.map_or(ExpectedFileState::Missing, |bytes| {
            ExpectedFileState::Present(FileFingerprint::from_bytes(&bytes))
        }),
    )
}

/// The replacement is written using `create_new` in the destination directory,
/// flushed to stable storage, and then atomically published. Existing files use
/// `rename(2)` on Unix and `MoveFileExW` on Windows.
pub(crate) fn replace_atomically(
    path: &Path,
    replacement: &[u8],
) -> Result<FileFingerprint, AtomicWriteError> {
    replace_atomically_inner(path, replacement, None)
}

pub(crate) fn replace_atomically_if_unchanged(
    path: &Path,
    replacement: &[u8],
    expected: ExpectedFileState,
) -> Result<FileFingerprint, AtomicWriteError> {
    replace_atomically_inner(path, replacement, Some(expected))
}

fn replace_atomically_inner(
    requested_path: &Path,
    replacement: &[u8],
    expected: Option<ExpectedFileState>,
) -> Result<FileFingerprint, AtomicWriteError> {
    let write_path = resolve_write_path(requested_path)?;
    let path = write_path.as_path();
    let parent = checked_parent(path)?;

    let existing_permissions = match fs::metadata(path) {
        Ok(metadata) => {
            validate_regular_file(path, &metadata)?;
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(io_error("inspect config permissions", path, source)),
    };

    let mut temp = create_temp_file(path, parent)?;
    if let Some(permissions) = existing_permissions {
        temp.file
            .as_ref()
            .expect("a newly-created temporary file is open")
            .set_permissions(permissions)
            .map_err(|source| io_error("preserve config permissions", &temp.path, source))?;
    } else {
        set_private_permissions(
            temp.file
                .as_ref()
                .expect("a newly-created temporary file is open"),
            &temp.path,
        )?;
    }
    temp.file
        .as_mut()
        .expect("a newly-created temporary file is open")
        .write_all(replacement)
        .map_err(|source| io_error("write temporary config", &temp.path, source))?;
    temp.file
        .as_ref()
        .expect("a newly-created temporary file is open")
        .sync_all()
        .map_err(|source| io_error("sync temporary config", &temp.path, source))?;
    drop(temp.file.take());

    if let Some(expected) = expected {
        let observed = observed_state(requested_path)?;
        if observed != expected {
            return Err(AtomicWriteError::Conflict { expected, observed });
        }
    }

    publish_temp(&temp.path, path)?;
    temp.keep = true;
    sync_directory(parent)?;

    Ok(FileFingerprint::from_bytes(replacement))
}

fn resolve_write_path(path: &Path) -> Result<PathBuf, AtomicWriteError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|source| io_error("resolve config symlink", path, source)),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(source) => Err(io_error("inspect config path", path, source)),
    }
}

pub(crate) fn remove_atomically(path: &Path) -> Result<(), AtomicWriteError> {
    let parent = checked_parent(path)?;
    match fs::remove_file(path) {
        Ok(()) => sync_directory(parent),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("remove config", path, source)),
    }
}

fn checked_parent(path: &Path) -> Result<&Path, AtomicWriteError> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| AtomicWriteError::MissingParent {
            path: path.to_path_buf(),
        })
}

fn validate_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<(), AtomicWriteError> {
    if !metadata.file_type().is_file() {
        return Err(AtomicWriteError::NonRegularFile {
            path: path.to_path_buf(),
        });
    }

    Ok(())
}

struct TempFile {
    path: PathBuf,
    file: Option<File>,
    keep: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_temp_file(path: &Path, parent: &Path) -> Result<TempFile, AtomicWriteError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let temp_path = parent.join(format!(".{file_name}.yaat-{}.tmp", Uuid::new_v4()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                return Ok(TempFile {
                    path: temp_path,
                    file: Some(file),
                    keep: false,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(io_error("create temporary config", &temp_path, source)),
        }
    }
    Err(AtomicWriteError::TempNameExhausted {
        path: path.to_path_buf(),
    })
}

#[cfg(unix)]
fn set_private_permissions(file: &File, path: &Path) -> Result<(), AtomicWriteError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error("set private config permissions", path, source))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &File, _path: &Path) -> Result<(), AtomicWriteError> {
    Ok(())
}

#[cfg(unix)]
fn publish_temp(temp_path: &Path, path: &Path) -> Result<(), AtomicWriteError> {
    fs::rename(temp_path, path)
        .map_err(|source| io_error("atomically replace config", path, source))
}

#[cfg(windows)]
fn publish_temp(temp_path: &Path, path: &Path) -> Result<(), AtomicWriteError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let temp_wide = wide(temp_path);
    let path_wide = wide(path);
    // SAFETY: Both buffers are owned, NUL-terminated UTF-16 strings that stay
    // alive for the call. `MoveFileExW` only reads the supplied pointers.
    let success = unsafe {
        MoveFileExW(
            temp_wide.as_ptr(),
            path_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        let source = io::Error::last_os_error();
        return Err(io_error("atomically publish config", path, source));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(parent: &Path) -> Result<(), AtomicWriteError> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error("sync config directory", parent, source))
}

#[cfg(windows)]
fn sync_directory(_parent: &Path) -> Result<(), AtomicWriteError> {
    // ReplaceFileW/MoveFileExW are called with their write-through flags.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
compile_error!("YAAT atomic config replacement is not implemented for this target");

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> AtomicWriteError {
    AtomicWriteError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_replace_leave_no_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");

        replace_atomically(&path, b"first").unwrap();
        let second = replace_atomically(&path, b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(second, FileFingerprint::from_bytes(b"second"));
        let entries = fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(entries, 1);
    }

    #[cfg(unix)]
    #[test]
    fn existing_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        fs::write(&path, b"before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        replace_atomically(&path, b"after").unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
