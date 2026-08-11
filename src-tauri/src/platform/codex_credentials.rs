// Codex credential-store interoperability.
//
// Codex supports file, direct-keyring, encrypted-secrets-keyring, auto, and
// ephemeral stores. YAAT detects the active keyring layout through Codex's
// `secret_auth_storage` feature and preserves unrelated top-level fields when
// switching account-owned fields.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use age::scrypt::{Identity as ScryptIdentity, Recipient as ScryptRecipient};
use age::secrecy::SecretString;
use base64::Engine as _;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;
use zeroize::{Zeroize, Zeroizing};

use crate::activation::{remove_atomically, replace_atomically};

use super::AdapterContext;

const AUTH_FILE_NAME: &str = "auth.json";
const CONFIG_FILE_NAME: &str = "config.toml";
const DIRECT_KEYRING_SERVICE: &str = "Codex Auth";
const SECRETS_KEYRING_SERVICE: &str = "codex";
const CODEX_AUTH_SECRET_KEY: &str = "global/CODEX_AUTH";
const CODEX_AUTH_SECRETS_FILE: &str = "codex_auth.age";
const MAX_AUTH_BYTES: u64 = 1024 * 1024;
const MAX_SECRETS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_KEYRING_VALUE_BYTES: usize = 4 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SECRETS_VERSION: u64 = 1;

const ACCOUNT_FIELDS: &[&str] = &["auth_mode", "OPENAI_API_KEY", "tokens", "last_refresh"];

const ACCOUNT_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "OPENAI_PROJECT",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialStoreMode {
    File,
    Keyring,
    Auto,
    Ephemeral,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyringBackendKind {
    Direct,
    Secrets,
}

trait KeyringAccess: Send + Sync {
    fn load(&self, service: &str, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, String>;
    fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String>;
    fn delete(&self, service: &str, account: &str) -> Result<bool, String>;
}

struct OsKeyring;

impl KeyringAccess for OsKeyring {
    fn load(&self, service: &str, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|error| format!("unable to open OS credential entry: {error}"))?;
        match entry.get_secret() {
            Ok(value) => {
                if value.len() > MAX_KEYRING_VALUE_BYTES {
                    return Err("OS credential entry exceeds the supported size".into());
                }
                Ok(Some(Zeroizing::new(value)))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!("unable to read OS credential entry: {error}")),
        }
    }

    fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        if value.is_empty() || value.len() > MAX_KEYRING_VALUE_BYTES {
            return Err("OS credential value has an unsupported size".into());
        }
        keyring::Entry::new(service, account)
            .map_err(|error| format!("unable to open OS credential entry: {error}"))?
            .set_secret(value)
            .map_err(|error| format!("unable to write OS credential entry: {error}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, String> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|error| format!("unable to open OS credential entry: {error}"))?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(format!("unable to delete OS credential entry: {error}")),
        }
    }
}

pub(crate) fn load(context: &AdapterContext, config_root: &Path) -> Result<Vec<u8>, String> {
    let mode = credential_store_mode(config_root)?;
    load_with_mode(context, config_root, mode, None, &OsKeyring)
}

pub(crate) fn load_optional(
    context: &AdapterContext,
    config_root: &Path,
) -> Result<Option<Vec<u8>>, String> {
    let mode = credential_store_mode(config_root)?;
    load_optional_with_mode(context, config_root, mode, None, &OsKeyring)
}

pub(crate) fn replace(
    context: &AdapterContext,
    config_root: &Path,
    target_snapshot: &[u8],
) -> Result<(), String> {
    let mode = credential_store_mode(config_root)?;
    replace_with_mode(
        context,
        config_root,
        mode,
        None,
        &OsKeyring,
        target_snapshot,
    )
}

pub(crate) fn clear_account_fields(
    context: &AdapterContext,
    config_root: &Path,
) -> Result<(), String> {
    let mode = credential_store_mode(config_root)?;
    clear_with_mode(context, config_root, mode, None, &OsKeyring)
}

/// Compare only the Codex account-owned fields. Newer unrelated credential
/// fields remain in the live document and are deliberately excluded.
pub(crate) fn account_fields_match(current: &[u8], snapshot: &[u8]) -> Result<bool, String> {
    let current = parse_auth_object(current, "current Codex credential")?;
    let snapshot = parse_auth_object(snapshot, "Codex credential snapshot")?;
    Ok(ACCOUNT_FIELDS
        .iter()
        .all(|field| current.get(*field) == snapshot.get(*field)))
}

pub(crate) fn account_fields_present(payload: &[u8]) -> Result<bool, String> {
    let payload = parse_auth_object(payload, "Codex credential")?;
    Ok(ACCOUNT_FIELDS
        .iter()
        .any(|field| payload.contains_key(*field)))
}

fn load_with_mode(
    context: &AdapterContext,
    config_root: &Path,
    mode: CredentialStoreMode,
    backend_override: Option<KeyringBackendKind>,
    keyring: &dyn KeyringAccess,
) -> Result<Vec<u8>, String> {
    let value = load_optional_with_mode(context, config_root, mode, backend_override, keyring)?;
    value.ok_or_else(|| match mode {
        CredentialStoreMode::File => format!(
            "Codex credential file {} is unavailable",
            config_root.join(AUTH_FILE_NAME).display()
        ),
        CredentialStoreMode::Keyring => format!(
            "Codex keyring credential is unavailable for {}",
            config_root.display()
        ),
        CredentialStoreMode::Auto => format!(
            "Codex auto credential storage is empty for {}",
            config_root.display()
        ),
        CredentialStoreMode::Ephemeral => {
            "Codex ephemeral credentials cannot be captured across processes".into()
        }
    })
}

fn load_optional_with_mode(
    context: &AdapterContext,
    config_root: &Path,
    mode: CredentialStoreMode,
    backend_override: Option<KeyringBackendKind>,
    keyring: &dyn KeyringAccess,
) -> Result<Option<Vec<u8>>, String> {
    ensure_existing_directory(config_root, "Codex home")?;
    match mode {
        CredentialStoreMode::File => read_optional_auth_file(config_root),
        CredentialStoreMode::Keyring => {
            let backend = resolve_backend(context, config_root, backend_override)?;
            load_keyring_backend(keyring, config_root, backend)
        }
        CredentialStoreMode::Auto => {
            let backend = resolve_backend(context, config_root, backend_override)?;
            match load_keyring_backend(keyring, config_root, backend) {
                Ok(Some(value)) => Ok(Some(value)),
                Ok(None) => read_optional_auth_file(config_root),
                Err(error) => Err(format!(
                    "Codex auto credential storage could not safely inspect its preferred {backend:?} keyring backend: {error}"
                )),
            }
        }
        CredentialStoreMode::Ephemeral => Err(
            "Codex ephemeral credentials cannot be captured or switched across processes".into(),
        ),
    }
}

fn replace_with_mode(
    context: &AdapterContext,
    config_root: &Path,
    mode: CredentialStoreMode,
    backend_override: Option<KeyringBackendKind>,
    keyring: &dyn KeyringAccess,
    target_snapshot: &[u8],
) -> Result<(), String> {
    ensure_existing_directory(config_root, "Codex home")?;
    let current = load_optional_with_mode(context, config_root, mode, backend_override, keyring)?;
    if current.is_none() && !account_fields_present(target_snapshot)? {
        return Ok(());
    }
    let replacement = merge_account_fields(current.as_deref().unwrap_or(b"{}"), target_snapshot)?;
    if current.as_deref() == Some(replacement.as_slice()) {
        return Ok(());
    }

    match mode {
        CredentialStoreMode::File => replace_file_auth(config_root, &replacement),
        CredentialStoreMode::Keyring | CredentialStoreMode::Auto => {
            let backend = resolve_backend(context, config_root, backend_override)?;
            match backend {
                KeyringBackendKind::Direct => {
                    replace_direct_keyring(keyring, config_root, &replacement)
                }
                KeyringBackendKind::Secrets => {
                    replace_secrets_keyring(keyring, config_root, &replacement)
                }
            }
        }
        CredentialStoreMode::Ephemeral => Err(
            "Codex ephemeral credentials cannot be captured or switched across processes".into(),
        ),
    }
}

fn clear_with_mode(
    context: &AdapterContext,
    config_root: &Path,
    mode: CredentialStoreMode,
    backend_override: Option<KeyringBackendKind>,
    keyring: &dyn KeyringAccess,
) -> Result<(), String> {
    ensure_existing_directory(config_root, "Codex home")?;
    let Some(current) =
        load_optional_with_mode(context, config_root, mode, backend_override, keyring)?
    else {
        return Ok(());
    };
    let replacement = merge_account_fields(&current, b"{}")?;
    if current == replacement {
        return Ok(());
    }
    if !parse_auth_object(&replacement, "cleared Codex credential")?.is_empty() {
        return replace_with_mode(context, config_root, mode, backend_override, keyring, b"{}");
    }

    match mode {
        CredentialStoreMode::File => remove_file_credential(config_root),
        CredentialStoreMode::Keyring => {
            let backend = resolve_backend(context, config_root, backend_override)?;
            remove_keyring_credential(keyring, config_root, backend)
        }
        CredentialStoreMode::Auto => {
            let backend = resolve_backend(context, config_root, backend_override)?;
            if load_keyring_backend(keyring, config_root, backend)?.is_some() {
                remove_keyring_credential(keyring, config_root, backend)
            } else {
                remove_file_credential(config_root)
            }
        }
        CredentialStoreMode::Ephemeral => Err(
            "Codex ephemeral credentials cannot be captured or switched across processes".into(),
        ),
    }
}

fn remove_file_credential(config_root: &Path) -> Result<(), String> {
    let path = config_root.join(AUTH_FILE_NAME);
    let previous = read_optional_auth_file(config_root)?;
    remove_atomically(&path)
        .map_err(|error| format!("failed to remove Codex credential file: {error}"))?;
    if read_optional_auth_file(config_root)?.is_none() {
        return Ok(());
    }
    let rollback = previous
        .as_deref()
        .map_or(Ok(()), |previous| restore_missing_file(&path, previous));
    Err(format!(
        "Codex credential file remained after removal; rollback: {}",
        result_detail(&rollback)
    ))
}

fn remove_keyring_credential(
    keyring: &dyn KeyringAccess,
    config_root: &Path,
    backend: KeyringBackendKind,
) -> Result<(), String> {
    match backend {
        KeyringBackendKind::Direct => {
            let account = direct_keyring_account(config_root);
            let previous = keyring.load(DIRECT_KEYRING_SERVICE, &account)?;
            keyring.delete(DIRECT_KEYRING_SERVICE, &account)?;
            if keyring.load(DIRECT_KEYRING_SERVICE, &account)?.is_none() {
                return Ok(());
            }
            let rollback = previous.as_deref().map_or(Ok(()), |previous| {
                keyring.save(DIRECT_KEYRING_SERVICE, &account, previous)
            });
            Err(format!(
                "Codex direct keyring credential remained after removal; rollback: {}",
                result_detail(&rollback)
            ))
        }
        KeyringBackendKind::Secrets => remove_secrets_credential(keyring, config_root),
    }
}

fn remove_secrets_credential(
    keyring: &dyn KeyringAccess,
    config_root: &Path,
) -> Result<(), String> {
    let path = secrets_auth_path(config_root);
    let Some(previous) =
        read_optional_regular_file(&path, MAX_SECRETS_BYTES, "Codex encrypted auth store")?
    else {
        return Ok(());
    };
    let account = secrets_keyring_account(config_root);
    let passphrase = keyring
        .load(SECRETS_KEYRING_SERVICE, &account)?
        .ok_or_else(|| {
            "Codex encrypted auth store exists but its Credential Manager/Keychain key is missing"
                .to_string()
        })?;
    let passphrase = std::str::from_utf8(&passphrase)
        .map_err(|_| "Codex encrypted auth-store key is not valid UTF-8".to_string())?;
    let mut document = decrypt_secrets_document(&previous, passphrase)?;
    document.remove_codex_auth()?;
    let plaintext = Zeroizing::new(
        serde_json::to_vec(document.value())
            .map_err(|error| format!("failed to serialize Codex encrypted auth store: {error}"))?,
    );
    let replacement = encrypt_secrets_document(&plaintext, passphrase)?;
    replace_atomically(&path, &replacement)
        .map_err(|error| format!("failed to clear Codex encrypted auth record: {error}"))?;
    if load_secrets_auth(keyring, config_root)?.is_none() {
        return Ok(());
    }
    let rollback = rollback_file(&path, Some(&previous));
    Err(format!(
        "Codex encrypted auth record remained after removal; rollback: {}",
        result_detail(&rollback)
    ))
}

fn merge_account_fields(current: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    let mut current = parse_auth_object(current, "current Codex credential")?;
    let target = parse_auth_object(target, "Codex credential snapshot")?;
    for field in ACCOUNT_FIELDS {
        current.remove(*field);
        if let Some(value) = target.get(*field) {
            current.insert((*field).to_string(), value.clone());
        }
    }
    serde_json::to_vec(&Value::Object(current))
        .map_err(|error| format!("failed to serialize merged Codex credential: {error}"))
}

fn parse_auth_object(payload: &[u8], label: &str) -> Result<Map<String, Value>, String> {
    if payload.is_empty() || payload.len() as u64 > MAX_AUTH_BYTES {
        return Err(format!("{label} has an unsupported size"));
    }
    let value: Value = serde_json::from_slice(payload)
        .map_err(|error| format!("{label} is not valid JSON: {error}"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{label} must be a JSON object"))
}

fn replace_file_auth(config_root: &Path, replacement: &[u8]) -> Result<(), String> {
    let path = config_root.join(AUTH_FILE_NAME);
    let previous = read_optional_auth_file(config_root)?;
    replace_atomically(&path, replacement)
        .map_err(|error| format!("failed to replace Codex credential file: {error}"))?;

    let verified =
        read_auth_file(config_root).and_then(|current| account_fields_match(&current, replacement));
    if matches!(verified, Ok(true)) {
        return Ok(());
    }

    let rollback = rollback_file(&path, previous.as_deref());
    Err(match rollback {
        Ok(()) => "Codex credential readback verification failed; the previous account-owned fields were restored".into(),
        Err(error) => format!(
            "Codex credential readback verification failed and rollback could not complete: {error}"
        ),
    })
}

fn replace_direct_keyring(
    keyring: &dyn KeyringAccess,
    config_root: &Path,
    replacement: &[u8],
) -> Result<(), String> {
    let account = direct_keyring_account(config_root);
    let previous_keyring = keyring.load(DIRECT_KEYRING_SERVICE, &account)?;
    let auth_path = config_root.join(AUTH_FILE_NAME);
    let previous_file = read_optional_auth_file(config_root)?;

    if let Err(error) = keyring.save(DIRECT_KEYRING_SERVICE, &account, replacement) {
        let rollback = rollback_keyring_if_applied(
            keyring,
            DIRECT_KEYRING_SERVICE,
            &account,
            replacement,
            previous_keyring.as_ref().map(|value| value.as_slice()),
        );
        return Err(with_rollback(
            format!("failed to write Codex direct keyring credential: {error}"),
            rollback,
        ));
    }

    if !keyring_value_matches(keyring, DIRECT_KEYRING_SERVICE, &account, replacement)? {
        let rollback = rollback_keyring_if_applied(
            keyring,
            DIRECT_KEYRING_SERVICE,
            &account,
            replacement,
            previous_keyring.as_ref().map(|value| value.as_slice()),
        );
        return Err(with_rollback(
            "Codex direct keyring readback verification failed".into(),
            rollback,
        ));
    }

    if let Some(previous_file) = previous_file.as_deref()
        && let Err(error) = remove_atomically(&auth_path)
    {
        let keyring_rollback = rollback_keyring_if_applied(
            keyring,
            DIRECT_KEYRING_SERVICE,
            &account,
            replacement,
            previous_keyring.as_ref().map(|value| value.as_slice()),
        );
        let file_rollback = restore_missing_file(&auth_path, previous_file);
        return Err(format!(
            "Codex auth.json cleanup failed: {error}; keyring rollback: {}; file rollback: {}",
            result_detail(&keyring_rollback),
            result_detail(&file_rollback)
        ));
    }

    if keyring_value_matches(keyring, DIRECT_KEYRING_SERVICE, &account, replacement)?
        && read_optional_auth_file(config_root)?.is_none()
    {
        return Ok(());
    }

    let keyring_rollback = rollback_keyring_if_applied(
        keyring,
        DIRECT_KEYRING_SERVICE,
        &account,
        replacement,
        previous_keyring.as_ref().map(|value| value.as_slice()),
    );
    let file_rollback = previous_file.as_deref().map_or(Ok(()), |previous| {
        restore_missing_file(&auth_path, previous)
    });
    Err(format!(
        "Codex direct keyring final verification failed; keyring rollback: {}; file rollback: {}",
        result_detail(&keyring_rollback),
        result_detail(&file_rollback)
    ))
}

fn replace_secrets_keyring(
    keyring: &dyn KeyringAccess,
    config_root: &Path,
    replacement: &[u8],
) -> Result<(), String> {
    let secrets_path = secrets_auth_path(config_root);
    let previous_encrypted = read_optional_regular_file(
        &secrets_path,
        MAX_SECRETS_BYTES,
        "Codex encrypted auth store",
    )?;
    let auth_path = config_root.join(AUTH_FILE_NAME);
    let previous_file = read_optional_auth_file(config_root)?;
    let passphrase_account = secrets_keyring_account(config_root);
    let mut passphrase = keyring.load(SECRETS_KEYRING_SERVICE, &passphrase_account)?;
    let mut created_passphrase = false;

    if previous_encrypted.is_some() && passphrase.is_none() {
        return Err(
            "Codex encrypted auth store exists but its Credential Manager/Keychain key is missing"
                .into(),
        );
    }
    if passphrase.is_none() {
        let generated = generate_passphrase()?;
        keyring.save(SECRETS_KEYRING_SERVICE, &passphrase_account, &generated)?;
        if !keyring_value_matches(
            keyring,
            SECRETS_KEYRING_SERVICE,
            &passphrase_account,
            &generated,
        )? {
            let _ = rollback_keyring_if_applied(
                keyring,
                SECRETS_KEYRING_SERVICE,
                &passphrase_account,
                &generated,
                None,
            );
            return Err("Codex encrypted auth-store key failed readback verification".into());
        }
        passphrase = Some(generated);
        created_passphrase = true;
    }
    let passphrase = passphrase.expect("created or loaded above");
    let passphrase_text = std::str::from_utf8(&passphrase)
        .map_err(|_| "Codex encrypted auth-store key is not valid UTF-8".to_string())?;
    if passphrase_text.is_empty() {
        return Err("Codex encrypted auth-store key is empty".into());
    }

    let mut document = match previous_encrypted.as_deref() {
        Some(encrypted) => decrypt_secrets_document(encrypted, passphrase_text)?,
        None => SensitiveJson::new_empty_secrets(),
    };
    document.set_codex_auth(replacement)?;
    let plaintext = Zeroizing::new(
        serde_json::to_vec(document.value())
            .map_err(|error| format!("failed to serialize Codex encrypted auth store: {error}"))?,
    );
    let encrypted = encrypt_secrets_document(&plaintext, passphrase_text)?;

    crate::paths::ensure_private_directory(
        secrets_path
            .parent()
            .ok_or_else(|| "Codex encrypted auth path has no parent".to_string())?,
    )
    .map_err(|error| format!("failed to create Codex secrets directory: {error}"))?;
    match replace_atomically(&secrets_path, &encrypted) {
        Ok(_) => {}
        Err(error) => {
            if created_passphrase {
                let _ = rollback_keyring_if_applied(
                    keyring,
                    SECRETS_KEYRING_SERVICE,
                    &passphrase_account,
                    &passphrase,
                    None,
                );
            }
            return Err(format!(
                "failed to replace Codex encrypted auth store: {error}"
            ));
        }
    }

    let verified = load_secrets_auth(keyring, config_root)
        .and_then(|current| current.ok_or_else(|| "encrypted auth record is missing".into()))
        .and_then(|current| account_fields_match(&current, replacement));
    if !matches!(verified, Ok(true)) {
        let file_rollback = rollback_file(&secrets_path, previous_encrypted.as_deref());
        let key_rollback = if created_passphrase {
            rollback_keyring_if_applied(
                keyring,
                SECRETS_KEYRING_SERVICE,
                &passphrase_account,
                &passphrase,
                None,
            )
        } else {
            Ok(())
        };
        return Err(format!(
            "Codex encrypted auth readback verification failed; file rollback: {}; key rollback: {}",
            result_detail(&file_rollback),
            result_detail(&key_rollback)
        ));
    }

    if let Some(previous_file) = previous_file.as_deref()
        && let Err(error) = remove_atomically(&auth_path)
    {
        let secrets_rollback = rollback_file(&secrets_path, previous_encrypted.as_deref());
        let auth_rollback = restore_missing_file(&auth_path, previous_file);
        let key_rollback = if created_passphrase {
            rollback_keyring_if_applied(
                keyring,
                SECRETS_KEYRING_SERVICE,
                &passphrase_account,
                &passphrase,
                None,
            )
        } else {
            Ok(())
        };
        return Err(format!(
            "Codex auth.json cleanup failed: {error}; encrypted-store rollback: {}; file rollback: {}; key rollback: {}",
            result_detail(&secrets_rollback),
            result_detail(&auth_rollback),
            result_detail(&key_rollback)
        ));
    }

    let final_verified = load_secrets_auth(keyring, config_root)
        .and_then(|current| current.ok_or_else(|| "encrypted auth record is missing".into()))
        .and_then(|current| account_fields_match(&current, replacement));
    if matches!(final_verified, Ok(true)) && read_optional_auth_file(config_root)?.is_none() {
        return Ok(());
    }

    let secrets_rollback = rollback_file(&secrets_path, previous_encrypted.as_deref());
    let auth_rollback = previous_file.as_deref().map_or(Ok(()), |previous| {
        restore_missing_file(&auth_path, previous)
    });
    let key_rollback = if created_passphrase {
        rollback_keyring_if_applied(
            keyring,
            SECRETS_KEYRING_SERVICE,
            &passphrase_account,
            &passphrase,
            None,
        )
    } else {
        Ok(())
    };
    Err(format!(
        "Codex encrypted auth final verification failed; encrypted-store rollback: {}; file rollback: {}; key rollback: {}",
        result_detail(&secrets_rollback),
        result_detail(&auth_rollback),
        result_detail(&key_rollback)
    ))
}

fn load_keyring_backend(
    keyring: &dyn KeyringAccess,
    config_root: &Path,
    backend: KeyringBackendKind,
) -> Result<Option<Vec<u8>>, String> {
    match backend {
        KeyringBackendKind::Direct => {
            let account = direct_keyring_account(config_root);
            keyring
                .load(DIRECT_KEYRING_SERVICE, &account)
                .map(|value| value.map(|value| value.to_vec()))
        }
        KeyringBackendKind::Secrets => load_secrets_auth(keyring, config_root),
    }
}

fn load_secrets_auth(
    keyring: &dyn KeyringAccess,
    config_root: &Path,
) -> Result<Option<Vec<u8>>, String> {
    let path = secrets_auth_path(config_root);
    let Some(encrypted) =
        read_optional_regular_file(&path, MAX_SECRETS_BYTES, "Codex encrypted auth store")?
    else {
        return Ok(None);
    };
    let account = secrets_keyring_account(config_root);
    let passphrase = keyring
        .load(SECRETS_KEYRING_SERVICE, &account)?
        .ok_or_else(|| {
            "Codex encrypted auth store exists but its Credential Manager/Keychain key is missing"
                .to_string()
        })?;
    let passphrase = std::str::from_utf8(&passphrase)
        .map_err(|_| "Codex encrypted auth-store key is not valid UTF-8".to_string())?;
    let document = decrypt_secrets_document(&encrypted, passphrase)?;
    document.codex_auth()
}

fn decrypt_secrets_document(ciphertext: &[u8], passphrase: &str) -> Result<SensitiveJson, String> {
    let identity = ScryptIdentity::new(SecretString::from(passphrase.to_owned()));
    let plaintext = Zeroizing::new(
        age::decrypt(&identity, ciphertext)
            .map_err(|error| format!("failed to decrypt Codex auth store: {error}"))?,
    );
    let value: Value = serde_json::from_slice(&plaintext)
        .map_err(|error| format!("Codex encrypted auth store is malformed: {error}"))?;
    let document = SensitiveJson(value);
    document.validate()?;
    Ok(document)
}

fn encrypt_secrets_document(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>, String> {
    let recipient = ScryptRecipient::new(SecretString::from(passphrase.to_owned()));
    age::encrypt(&recipient, plaintext)
        .map_err(|error| format!("failed to encrypt Codex auth store: {error}"))
}

struct SensitiveJson(Value);

impl SensitiveJson {
    fn new_empty_secrets() -> Self {
        Self(serde_json::json!({
            "version": SECRETS_VERSION,
            "secrets": {}
        }))
    }

    fn value(&self) -> &Value {
        &self.0
    }

    fn validate(&self) -> Result<(), String> {
        let object = self
            .0
            .as_object()
            .ok_or_else(|| "Codex encrypted auth store must be an object".to_string())?;
        let version = object
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| "Codex encrypted auth store has no valid version".to_string())?;
        if version > SECRETS_VERSION {
            return Err(format!(
                "Codex encrypted auth store version {version} is newer than supported version {SECRETS_VERSION}"
            ));
        }
        if !object.get("secrets").is_some_and(Value::is_object) {
            return Err("Codex encrypted auth store has no secrets object".into());
        }
        Ok(())
    }

    fn codex_auth(&self) -> Result<Option<Vec<u8>>, String> {
        self.validate()?;
        let value = self
            .0
            .pointer("/secrets/global~1CODEX_AUTH")
            .and_then(Value::as_str);
        match value {
            Some(value) if value.len() as u64 <= MAX_AUTH_BYTES => {
                Ok(Some(value.as_bytes().to_vec()))
            }
            Some(_) => Err("Codex encrypted auth record exceeds the supported size".into()),
            None => Ok(None),
        }
    }

    fn set_codex_auth(&mut self, payload: &[u8]) -> Result<(), String> {
        self.validate()?;
        let payload = std::str::from_utf8(payload)
            .map_err(|_| "Codex credential payload is not valid UTF-8".to_string())?;
        let object = self
            .0
            .as_object_mut()
            .expect("validated as an object above");
        if object.get("version").and_then(Value::as_u64) == Some(0) {
            object.insert("version".into(), Value::from(SECRETS_VERSION));
        }
        object
            .get_mut("secrets")
            .and_then(Value::as_object_mut)
            .expect("validated secrets object above")
            .insert(
                CODEX_AUTH_SECRET_KEY.into(),
                Value::String(payload.to_owned()),
            );
        Ok(())
    }

    fn remove_codex_auth(&mut self) -> Result<(), String> {
        self.validate()?;
        self.0
            .get_mut("secrets")
            .and_then(Value::as_object_mut)
            .expect("validated secrets object above")
            .remove(CODEX_AUTH_SECRET_KEY);
        Ok(())
    }
}

impl Drop for SensitiveJson {
    fn drop(&mut self) {
        zeroize_json(&mut self.0);
    }
}

fn zeroize_json(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(zeroize_json),
        Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                key.zeroize();
                zeroize_json(&mut value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn generate_passphrase() -> Result<Zeroizing<Vec<u8>>, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("failed to generate Codex auth-store key: {error}"))?;
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .into_bytes();
    bytes.zeroize();
    Ok(Zeroizing::new(encoded))
}

fn rollback_keyring_if_applied(
    keyring: &dyn KeyringAccess,
    service: &str,
    account: &str,
    _applied: &[u8],
    previous: Option<&[u8]>,
) -> Result<(), String> {
    match previous {
        Some(previous) => keyring.save(service, account, previous)?,
        None => {
            keyring.delete(service, account)?;
        }
    }
    let current = keyring.load(service, account)?;
    if current.as_ref().map(|value| value.as_slice()) == previous {
        Ok(())
    } else {
        Err("OS credential rollback could not be verified".into())
    }
}

fn keyring_value_matches(
    keyring: &dyn KeyringAccess,
    service: &str,
    account: &str,
    expected: &[u8],
) -> Result<bool, String> {
    Ok(keyring
        .load(service, account)?
        .as_ref()
        .map(|value| value.as_slice())
        == Some(expected))
}

fn rollback_file(path: &Path, previous: Option<&[u8]>) -> Result<(), String> {
    previous.map_or_else(
        || remove_atomically(path).map_err(|error| error.to_string()),
        |previous| {
            replace_atomically(path, previous)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
}

fn restore_missing_file(path: &Path, previous: &[u8]) -> Result<(), String> {
    replace_atomically(path, previous)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn with_rollback(message: String, rollback: Result<(), String>) -> String {
    format!("{message}; rollback: {}", result_detail(&rollback))
}

fn result_detail(result: &Result<(), String>) -> &str {
    match result {
        Ok(()) => "verified",
        Err(_) => "failed",
    }
}

fn read_auth_file(config_root: &Path) -> Result<Vec<u8>, String> {
    read_optional_auth_file(config_root)?.ok_or_else(|| {
        format!(
            "Codex credential file {} is unavailable",
            config_root.join(AUTH_FILE_NAME).display()
        )
    })
}

fn read_optional_auth_file(config_root: &Path) -> Result<Option<Vec<u8>>, String> {
    read_optional_regular_file(
        &config_root.join(AUTH_FILE_NAME),
        MAX_AUTH_BYTES,
        "Codex credential file",
    )
}

fn read_optional_regular_file(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Option<Vec<u8>>, String> {
    let link_metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "{label} {} is unavailable: {error}",
                path.display()
            ));
        }
    };
    if !link_metadata.is_file() {
        return Err(format!("{label} path {} is not a file", path.display()));
    }
    if link_metadata.len() > max_bytes {
        return Err(format!(
            "{label} {} exceeds the supported {max_bytes} byte limit",
            path.display()
        ));
    }
    let file = File::open(path)
        .map_err(|error| format!("failed to open {label} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect open {label} {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(format!("{label} changed while it was being opened"));
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| format!("{label} is too large for this platform"))?;
    let mut payload = Vec::with_capacity(capacity);
    file.take(max_bytes + 1)
        .read_to_end(&mut payload)
        .map_err(|error| format!("failed to read {label} {}: {error}", path.display()))?;
    if payload.len() as u64 > max_bytes {
        payload.zeroize();
        return Err(format!(
            "{label} grew beyond the supported limit while reading"
        ));
    }
    Ok(Some(payload))
}

fn credential_store_mode(config_root: &Path) -> Result<CredentialStoreMode, String> {
    let source = read_optional_config(&config_root.join(CONFIG_FILE_NAME))?;
    if source.trim().is_empty() {
        return Ok(CredentialStoreMode::File);
    }
    let doc = source.parse::<DocumentMut>().map_err(|error| {
        format!(
            "cannot determine Codex credential storage because config.toml is malformed: {error}"
        )
    })?;
    match doc.get("cli_auth_credentials_store") {
        None => Ok(CredentialStoreMode::File),
        Some(item) => match item.as_str() {
            Some("file") => Ok(CredentialStoreMode::File),
            Some("keyring") => Ok(CredentialStoreMode::Keyring),
            Some("auto") => Ok(CredentialStoreMode::Auto),
            Some("ephemeral") => Ok(CredentialStoreMode::Ephemeral),
            Some(other) => Err(format!(
                "unsupported Codex cli_auth_credentials_store value `{other}`"
            )),
            None => Err("Codex cli_auth_credentials_store must be a string".into()),
        },
    }
}

fn read_optional_config(path: &Path) -> Result<String, String> {
    let Some(bytes) = read_optional_regular_file(path, MAX_SECRETS_BYTES, "Codex config")? else {
        return Ok(String::new());
    };
    String::from_utf8(bytes)
        .map_err(|_| format!("Codex config {} is not valid UTF-8", path.display()))
}

fn resolve_backend(
    context: &AdapterContext,
    config_root: &Path,
    backend_override: Option<KeyringBackendKind>,
) -> Result<KeyringBackendKind, String> {
    if let Some(backend) = backend_override {
        return Ok(backend);
    }
    let program = resolve_cli_path(context)?;
    let feature_output = run_codex(&program, &["features", "list"], config_root)?;
    let features = command_text(feature_output, "Codex feature list")?;
    let enabled = features.lines().find_map(|line| {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        (columns.first().copied() == Some("secret_auth_storage"))
            .then(|| columns.last().copied())
            .flatten()
    });
    match enabled {
        Some("true") => Ok(KeyringBackendKind::Secrets),
        Some("false") => Ok(KeyringBackendKind::Direct),
        Some(other) => Err(format!(
            "Codex returned invalid secret_auth_storage state `{other}`"
        )),
        None => Err(
            "Codex did not report the secret_auth_storage feature; refusing to guess its keyring layout"
                .into(),
        ),
    }
}

fn resolve_cli_path(context: &AdapterContext) -> Result<PathBuf, String> {
    super::executable::resolve(
        super::executable::CliProgram::Codex,
        context.explicit_cli_path.as_deref(),
    )
}

fn run_codex(
    program: &Path,
    args: &[&str],
    config_root: &Path,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut command = Command::new(program);
    crate::process::configure_background(&mut command);
    command
        .args(args)
        .env("CODEX_HOME", config_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in ACCOUNT_ENV_VARS {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start {}: {error}", program.display()))?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} did not answer within {} seconds",
                    program.display(),
                    COMMAND_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "failed while waiting for {}: {error}",
                    program.display()
                ));
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        pipe.take(256 * 1024)
            .read_to_end(&mut stdout)
            .map_err(|error| format!("failed to read Codex output: {error}"))?;
    }
    if let Some(pipe) = child.stderr.take() {
        pipe.take(256 * 1024)
            .read_to_end(&mut stderr)
            .map_err(|error| format!("failed to read Codex error output: {error}"))?;
    }
    Ok((status, stdout, stderr))
}

fn command_text(
    (status, stdout, stderr): (ExitStatus, Vec<u8>, Vec<u8>),
    label: &str,
) -> Result<String, String> {
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("{label} command exited with {status}")
        } else {
            format!("{label} command failed: {detail}")
        });
    }
    let bytes = if stdout.is_empty() { stderr } else { stdout };
    String::from_utf8(bytes).map_err(|_| format!("{label} output was not UTF-8"))
}

fn direct_keyring_account(config_root: &Path) -> String {
    format!("cli|{}", path_hash(config_root))
}

fn secrets_keyring_account(config_root: &Path) -> String {
    format!("secrets|{}", path_hash(config_root))
}

fn path_hash(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hex = format!("{digest:x}");
    hex.get(..16).unwrap_or(&hex).to_string()
}

fn secrets_auth_path(config_root: &Path) -> PathBuf {
    config_root.join("secrets").join(CODEX_AUTH_SECRETS_FILE)
}

fn ensure_existing_directory(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("{label} {} is unavailable: {error}", path.display()))?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(format!("{label} {} is not a directory", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::*;

    #[derive(Default)]
    struct MockKeyring {
        values: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    }

    impl MockKeyring {
        fn put(&self, service: &str, account: &str, value: &[u8]) {
            self.values
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), value.to_vec());
        }

        fn get(&self, service: &str, account: &str) -> Option<Vec<u8>> {
            self.values
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned()
        }
    }

    impl KeyringAccess for MockKeyring {
        fn load(&self, service: &str, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
            Ok(self.get(service, account).map(Zeroizing::new))
        }

        fn save(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
            self.put(service, account, value);
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<bool, String> {
            Ok(self
                .values
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()))
                .is_some())
        }
    }

    fn context(temp: &TempDir) -> AdapterContext {
        AdapterContext {
            data_root: temp.path().join("data"),
            explicit_cli_path: None,
            explicit_config_root: None,
        }
    }

    fn auth(account: &str, unowned: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": format!("id-{account}"),
                "access_token": format!("access-{account}"),
                "refresh_token": format!("refresh-{account}"),
                "account_id": account
            },
            "last_refresh": "2026-08-02T00:00:00Z",
            "agent_identity": {"private": unowned},
            "future_unowned": unowned
        }))
        .unwrap()
    }

    #[test]
    fn file_switch_creates_a_missing_credential_slot() {
        let temp = TempDir::new().unwrap();
        replace_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::File,
            None,
            &MockKeyring::default(),
            &auth("account-a", "source-only"),
        )
        .unwrap();

        let stored = read_auth_file(temp.path()).unwrap();
        assert!(account_fields_match(&stored, &auth("account-a", "ignored")).unwrap());
        let stored: Value = serde_json::from_slice(&stored).unwrap();
        assert!(stored.get("agent_identity").is_none());
        assert!(stored.get("future_unowned").is_none());
    }

    #[test]
    fn clearing_file_account_fields_preserves_unowned_fields() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(AUTH_FILE_NAME), auth("account-a", "keep")).unwrap();

        replace_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::File,
            None,
            &MockKeyring::default(),
            b"{}",
        )
        .unwrap();

        let stored = read_auth_file(temp.path()).unwrap();
        assert!(!account_fields_present(&stored).unwrap());
        let stored: Value = serde_json::from_slice(&stored).unwrap();
        assert_eq!(stored["agent_identity"]["private"], "keep");
        assert_eq!(stored["future_unowned"], "keep");
    }

    #[test]
    fn clearing_an_already_missing_slot_is_a_noop() {
        let temp = TempDir::new().unwrap();
        replace_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::File,
            None,
            &MockKeyring::default(),
            b"{}",
        )
        .unwrap();
        assert!(!temp.path().join(AUTH_FILE_NAME).exists());
    }

    #[test]
    fn clearing_an_account_only_file_removes_the_slot() {
        let temp = TempDir::new().unwrap();
        let account_only = merge_account_fields(b"{}", &auth("account-a", "ignored")).unwrap();
        fs::write(temp.path().join(AUTH_FILE_NAME), account_only).unwrap();

        clear_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::File,
            None,
            &MockKeyring::default(),
        )
        .unwrap();
        assert!(!temp.path().join(AUTH_FILE_NAME).exists());
    }

    #[test]
    fn clearing_an_account_only_direct_keyring_removes_the_entry() {
        let temp = TempDir::new().unwrap();
        let keyring = MockKeyring::default();
        let account = direct_keyring_account(temp.path());
        let account_only = merge_account_fields(b"{}", &auth("account-a", "ignored")).unwrap();
        keyring.put(DIRECT_KEYRING_SERVICE, &account, &account_only);

        clear_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Keyring,
            Some(KeyringBackendKind::Direct),
            &keyring,
        )
        .unwrap();
        assert!(keyring.get(DIRECT_KEYRING_SERVICE, &account).is_none());
    }

    #[test]
    fn direct_keyring_switch_preserves_unowned_fields_and_removes_file_fallback() {
        let temp = TempDir::new().unwrap();
        let keyring = MockKeyring::default();
        let account = direct_keyring_account(temp.path());
        keyring.put(
            DIRECT_KEYRING_SERVICE,
            &account,
            &auth("account-a", "keep-a"),
        );
        fs::write(
            temp.path().join(AUTH_FILE_NAME),
            auth("stale-file", "stale"),
        )
        .unwrap();

        replace_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Keyring,
            Some(KeyringBackendKind::Direct),
            &keyring,
            &auth("account-b", "must-not-copy"),
        )
        .unwrap();

        let stored: Value = serde_json::from_slice(
            &keyring
                .get(DIRECT_KEYRING_SERVICE, &account)
                .expect("direct keyring value"),
        )
        .unwrap();
        assert_eq!(stored["tokens"]["account_id"], "account-b");
        assert_eq!(stored["agent_identity"]["private"], "keep-a");
        assert_eq!(stored["future_unowned"], "keep-a");
        assert!(!temp.path().join(AUTH_FILE_NAME).exists());
    }

    #[test]
    fn auto_file_fallback_is_migrated_to_verified_direct_keyring() {
        let temp = TempDir::new().unwrap();
        let keyring = MockKeyring::default();
        fs::write(
            temp.path().join(AUTH_FILE_NAME),
            auth("account-a", "keep-a"),
        )
        .unwrap();

        let loaded = load_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Auto,
            Some(KeyringBackendKind::Direct),
            &keyring,
        )
        .unwrap();
        assert!(account_fields_match(&loaded, &auth("account-a", "ignored")).unwrap());

        replace_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Auto,
            Some(KeyringBackendKind::Direct),
            &keyring,
            &auth("account-b", "must-not-copy"),
        )
        .unwrap();
        assert!(!temp.path().join(AUTH_FILE_NAME).exists());
        let stored = keyring
            .get(DIRECT_KEYRING_SERVICE, &direct_keyring_account(temp.path()))
            .unwrap();
        let stored: Value = serde_json::from_slice(&stored).unwrap();
        assert_eq!(stored["tokens"]["account_id"], "account-b");
        assert_eq!(stored["agent_identity"]["private"], "keep-a");
    }

    #[test]
    fn encrypted_secrets_switch_preserves_other_secrets_and_auth_fields() {
        let temp = TempDir::new().unwrap();
        let keyring = MockKeyring::default();
        let passphrase = b"test-passphrase-with-enough-entropy";
        let passphrase_account = secrets_keyring_account(temp.path());
        keyring.put(SECRETS_KEYRING_SERVICE, &passphrase_account, passphrase);

        let mut document = SensitiveJson::new_empty_secrets();
        document
            .set_codex_auth(&auth("account-a", "keep-a"))
            .unwrap();
        document
            .0
            .pointer_mut("/secrets")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "global/OTHER_SECRET".into(),
                Value::String("keep-me".into()),
            );
        let plaintext = serde_json::to_vec(document.value()).unwrap();
        let encrypted =
            encrypt_secrets_document(&plaintext, std::str::from_utf8(passphrase).unwrap()).unwrap();
        let secrets_dir = temp.path().join("secrets");
        fs::create_dir(&secrets_dir).unwrap();
        fs::write(secrets_dir.join(CODEX_AUTH_SECRETS_FILE), encrypted).unwrap();

        replace_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Keyring,
            Some(KeyringBackendKind::Secrets),
            &keyring,
            &auth("account-b", "must-not-copy"),
        )
        .unwrap();

        let loaded = load_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Keyring,
            Some(KeyringBackendKind::Secrets),
            &keyring,
        )
        .unwrap();
        let loaded: Value = serde_json::from_slice(&loaded).unwrap();
        assert_eq!(loaded["tokens"]["account_id"], "account-b");
        assert_eq!(loaded["agent_identity"]["private"], "keep-a");

        let encrypted = fs::read(secrets_dir.join(CODEX_AUTH_SECRETS_FILE)).unwrap();
        let document =
            decrypt_secrets_document(&encrypted, std::str::from_utf8(passphrase).unwrap()).unwrap();
        assert_eq!(
            document.value()["secrets"]["global/OTHER_SECRET"],
            "keep-me"
        );
    }

    #[test]
    fn encrypted_store_with_missing_key_fails_closed() {
        let temp = TempDir::new().unwrap();
        let keyring = MockKeyring::default();
        let secrets_dir = temp.path().join("secrets");
        fs::create_dir(&secrets_dir).unwrap();
        fs::write(
            secrets_dir.join(CODEX_AUTH_SECRETS_FILE),
            b"encrypted-but-key-is-missing",
        )
        .unwrap();

        let error = load_with_mode(
            &context(&temp),
            temp.path(),
            CredentialStoreMode::Keyring,
            Some(KeyringBackendKind::Secrets),
            &keyring,
        )
        .unwrap_err();
        assert!(error.contains("key is missing"));
    }
}
