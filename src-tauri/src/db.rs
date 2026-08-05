//! SQLite persistence for YAAT.
//!
//! Account metadata and credentials are stored in the local database. Secret
//! wrapper types still keep credential values out of debug output and ordinary
//! profile responses; explicit reveal is assembled at the Tauri command boundary.

#[path = "db/migrations.rs"]
mod migrations;

use std::{
    fmt,
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use secrecy::{ExposeSecret, SecretSlice, SecretString};
use uuid::Uuid;
use yaat_contracts::{
    ActivationMode, AppSettings, CreateProviderRequest, DeleteProviderRequest, Platform,
    PlatformBinding, ProfileStatus, ProviderKind, ProviderProfile, SecretKind, TokenBreakdown,
    UpdateProviderRequest, UsageDiagnostics,
};

const DEFAULT_LANGUAGE: &str = "system";
const DEFAULT_THEME: &str = "auto";
const DEFAULT_TIMEZONE: &str = "UTC";
const CREDENTIAL_RECORD_NAME: &str = "credential";

/// Thread-safe repository suitable for Tauri managed state.
pub struct Repository {
    connection: Mutex<Connection>,
}

#[derive(Clone, Copy)]
enum BindingColumn {
    Global,
    LastManaged,
}

impl fmt::Debug for Repository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Repository").finish_non_exhaustive()
    }
}

impl Repository {
    /// Open or create the local repository.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    fn from_connection(mut connection: Connection) -> Result<Self, DbError> {
        configure_connection(&connection)?;
        migrations::apply(&mut connection)?;
        ensure_settings_row(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self, DbError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    pub fn list_providers(
        &self,
        platform: Option<Platform>,
    ) -> Result<Vec<ProviderProfile>, DbError> {
        let connection = self.lock()?;
        let rows = if let Some(platform) = platform {
            let mut statement = connection.prepare(&format!(
                "{PROVIDER_SELECT} WHERE p.platform = ?1 ORDER BY p.created_at, p.id"
            ))?;
            statement
                .query_map([platform.as_str()], provider_row)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut statement = connection.prepare(&format!(
                "{PROVIDER_SELECT} ORDER BY p.platform, p.created_at, p.id"
            ))?;
            statement
                .query_map([], provider_row)?
                .collect::<Result<Vec<_>, _>>()?
        };

        rows.into_iter()
            .map(|row| self.hydrate_provider(&connection, row))
            .collect()
    }

    pub fn get_provider(&self, id: &str) -> Result<Option<ProviderProfile>, DbError> {
        let connection = self.lock()?;
        let row = fetch_provider_row(&connection, id)?;
        row.map(|row| self.hydrate_provider(&connection, row))
            .transpose()
    }

    pub fn create_provider(
        &self,
        request: &CreateProviderRequest,
    ) -> Result<ProviderProfile, DbError> {
        validate_provider_name(&request.name)?;
        validate_secret_input(request.secret_kind, request.secret.as_deref())?;

        let id = Uuid::new_v4().to_string();
        let now = now_millis();
        let status = initial_status(request.kind, request.secret_kind, request.secret.as_deref());
        let credential = match request.secret.as_deref() {
            Some(secret) => Some(self.prepare_text_data(
                &id,
                &credential_record_id(&id),
                credential_kind(request.secret_kind)?,
                Some(&id),
                secret,
            )?),
            None => None,
        };

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO providers( \
                id, platform, kind, name, account_label, base_url, model, secret_kind, status, created_at, updated_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                id,
                request.platform.as_str(),
                provider_kind_to_db(request.kind),
                request.name.trim(),
                request.account_label,
                request.base_url,
                request.model,
                secret_kind_to_db(request.secret_kind),
                profile_status_to_db(status),
                now,
            ],
        )?;
        if let Some(record) = credential.as_ref() {
            upsert_account_data(&transaction, record, now)?;
        }
        transaction.commit()?;
        drop(connection);

        self.get_provider(&id)?.ok_or_else(|| DbError::NotFound {
            entity: "provider",
            id,
        })
    }

    pub fn update_provider(
        &self,
        request: &UpdateProviderRequest,
    ) -> Result<ProviderProfile, DbError> {
        validate_provider_name(&request.name)?;
        validate_secret_input(request.secret_kind, request.replacement_secret.as_deref())?;

        let mut connection = self.lock()?;
        let current =
            fetch_provider_row(&connection, &request.id)?.ok_or_else(|| DbError::NotFound {
                entity: "provider",
                id: request.id.clone(),
            })?;

        let credential_record_id = credential_record_id(&request.id);
        let current_secret_kind = secret_kind_from_db(&current.secret_kind)?;
        let credential = self.prepare_credential_update(
            &connection,
            &request.id,
            current_secret_kind,
            request.secret_kind,
            request.replacement_secret.as_deref(),
        )?;
        let now = now_millis();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE providers SET \
                name = ?1, account_label = ?2, base_url = ?3, model = ?4, secret_kind = ?5, \
                updated_at = ?6 \
             WHERE id = ?7",
            params![
                request.name.trim(),
                request.account_label,
                request.base_url,
                request.model,
                secret_kind_to_db(request.secret_kind),
                now,
                request.id,
            ],
        )?;
        if changed != 1 {
            return Err(DbError::NotFound {
                entity: "provider",
                id: request.id.clone(),
            });
        }
        match credential {
            CredentialUpdate::Keep => {}
            CredentialUpdate::Replace(record) => upsert_account_data(&transaction, &record, now)?,
            CredentialUpdate::Delete => {
                transaction.execute(
                    "DELETE FROM account_data WHERE record_id = ?1",
                    [&credential_record_id],
                )?;
            }
        }
        transaction.commit()?;
        drop(connection);

        self.get_provider(&request.id)?
            .ok_or_else(|| DbError::NotFound {
                entity: "provider",
                id: request.id.clone(),
            })
    }

    pub fn delete_provider(&self, request: &DeleteProviderRequest) -> Result<(), DbError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_platform: Option<String> = transaction
            .query_row(
                "SELECT platform FROM platform_bindings WHERE global_profile_id = ?1 LIMIT 1",
                [&request.id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(platform) = active_platform {
            return Err(DbError::ProviderActive {
                id: request.id.clone(),
                platform,
            });
        }

        let now = now_millis();
        transaction.execute(
            "UPDATE platform_bindings SET last_managed_profile_id = NULL, updated_at = ?1 \
             WHERE last_managed_profile_id = ?2",
            params![now, request.id],
        )?;

        transaction.execute(
            "UPDATE settings SET unify_claude_desktop_code_history = 0, \
                 claude_desktop_history_target = NULL, updated_at = ?1 \
             WHERE claude_desktop_history_target LIKE ?2",
            params![now, format!("profile:{}:%", request.id)],
        )?;

        let changed = transaction.execute("DELETE FROM providers WHERE id = ?1", [&request.id])?;
        if changed != 1 {
            return Err(DbError::NotFound {
                entity: "provider",
                id: request.id.clone(),
            });
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn get_platform_binding(&self, platform: Platform) -> Result<PlatformBinding, DbError> {
        let connection = self.lock()?;
        fetch_platform_binding(&connection, platform)?.ok_or_else(|| DbError::NotFound {
            entity: "platform_binding",
            id: platform.as_str().to_owned(),
        })
    }

    pub fn set_global_profile(
        &self,
        platform: Platform,
        profile_id: Option<&str>,
    ) -> Result<PlatformBinding, DbError> {
        self.set_binding_profile(platform, profile_id, BindingColumn::Global)
    }

    pub fn set_last_managed_profile(
        &self,
        platform: Platform,
        profile_id: Option<&str>,
    ) -> Result<PlatformBinding, DbError> {
        self.set_binding_profile(platform, profile_id, BindingColumn::LastManaged)
    }

    fn set_binding_profile(
        &self,
        platform: Platform,
        profile_id: Option<&str>,
        column: BindingColumn,
    ) -> Result<PlatformBinding, DbError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(profile_id) = profile_id {
            let provider_platform: Option<String> = transaction
                .query_row(
                    "SELECT platform FROM providers WHERE id = ?1",
                    [profile_id],
                    |row| row.get(0),
                )
                .optional()?;
            match provider_platform {
                None => {
                    return Err(DbError::NotFound {
                        entity: "provider",
                        id: profile_id.to_owned(),
                    });
                }
                Some(actual) if actual != platform.as_str() => {
                    return Err(DbError::PlatformMismatch {
                        profile_id: profile_id.to_owned(),
                        expected: platform.as_str().to_owned(),
                        actual,
                    });
                }
                Some(_) => {}
            }
        }

        let sql = match column {
            BindingColumn::Global => {
                "UPDATE platform_bindings SET global_profile_id = ?1, updated_at = ?2 WHERE platform = ?3"
            }
            BindingColumn::LastManaged => {
                "UPDATE platform_bindings SET last_managed_profile_id = ?1, updated_at = ?2 WHERE platform = ?3"
            }
        };
        let changed =
            transaction.execute(sql, params![profile_id, now_millis(), platform.as_str()])?;
        if changed != 1 {
            return Err(DbError::NotFound {
                entity: "platform_binding",
                id: platform.as_str().to_owned(),
            });
        }
        transaction.commit()?;
        drop(connection);
        self.get_platform_binding(platform)
    }

    pub fn load_settings(&self) -> Result<AppSettings, DbError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT language, theme, timezone, default_activation_mode, \
                    codex_path, claude_path, claude_desktop_path, codex_home, \
                    claude_config_dir, unify_codex_history, unify_claude_code_history, \
                    unify_claude_desktop_code_history, claude_desktop_history_target \
             FROM settings WHERE singleton_id = 1",
                [],
                |row| {
                    Ok(SettingsRow {
                        language: row.get(0)?,
                        theme: row.get(1)?,
                        timezone: row.get(2)?,
                        default_activation_mode: row.get(3)?,
                        codex_path: row.get(4)?,
                        claude_path: row.get(5)?,
                        claude_desktop_path: row.get(6)?,
                        codex_home: row.get(7)?,
                        claude_config_dir: row.get(8)?,
                        unify_codex_history: row.get(9)?,
                        unify_claude_code_history: row.get(10)?,
                        unify_claude_desktop_code_history: row.get(11)?,
                        claude_desktop_history_target: row.get(12)?,
                    })
                },
            )
            .map_err(DbError::from)
            .and_then(settings_from_row)
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<AppSettings, DbError> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE settings SET \
                language = ?1, theme = ?2, timezone = ?3, default_activation_mode = ?4, \
                codex_path = ?5, claude_path = ?6, claude_desktop_path = ?7, \
                codex_home = ?8, claude_config_dir = ?9, \
                unify_codex_history = ?10, unify_claude_code_history = ?11, \
                unify_claude_desktop_code_history = ?12, claude_desktop_history_target = ?13, \
                updated_at = ?14 WHERE singleton_id = 1",
            params![
                settings.language,
                settings.theme,
                settings.timezone,
                activation_mode_to_db(settings.default_activation_mode),
                settings.codex_path,
                settings.claude_path,
                settings.claude_desktop_path,
                settings.codex_home,
                settings.claude_config_dir,
                bool_to_db(settings.unify_codex_history),
                bool_to_db(settings.unify_claude_code_history),
                bool_to_db(settings.unify_claude_desktop_code_history),
                settings.claude_desktop_history_target,
                now_millis(),
            ],
        )?;
        if changed != 1 {
            return Err(DbError::NotFound {
                entity: "settings",
                id: "1".to_owned(),
            });
        }
        drop(connection);
        self.load_settings()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, DbError> {
        self.connection.lock().map_err(|_| DbError::LockPoisoned)
    }

    fn hydrate_provider(
        &self,
        _connection: &Connection,
        row: ProviderRow,
    ) -> Result<ProviderProfile, DbError> {
        Ok(ProviderProfile {
            id: row.id,
            platform: platform_from_db(&row.platform)?,
            kind: provider_kind_from_db(&row.kind)?,
            name: row.name,
            account_label: row.account_label,
            base_url: row.base_url,
            model: row.model,
            secret_kind: secret_kind_from_db(&row.secret_kind)?,
            has_secret: row.has_secret,
            profile_home: row.profile_home,
            status: profile_status_from_db(&row.status)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    fn prepare_text_data(
        &self,
        profile_id: &str,
        record_id: &str,
        kind: &str,
        provider_id: Option<&str>,
        value: &str,
    ) -> Result<PreparedAccountData, DbError> {
        self.prepare_account_data(profile_id, record_id, kind, provider_id, value.as_bytes())
    }

    fn prepare_account_data(
        &self,
        profile_id: &str,
        record_id: &str,
        kind: &str,
        provider_id: Option<&str>,
        value: &[u8],
    ) -> Result<PreparedAccountData, DbError> {
        validate_sensitive_locator(profile_id, record_id, kind)?;
        Ok(PreparedAccountData {
            profile_id: profile_id.to_owned(),
            record_id: record_id.to_owned(),
            kind: kind.to_owned(),
            provider_id: provider_id.map(str::to_owned),
            value: value.to_vec(),
        })
    }

    fn prepare_credential_update(
        &self,
        connection: &Connection,
        profile_id: &str,
        old_kind: SecretKind,
        new_kind: SecretKind,
        replacement: Option<&str>,
    ) -> Result<CredentialUpdate, DbError> {
        if new_kind == SecretKind::None {
            return Ok(CredentialUpdate::Delete);
        }
        let record_id = credential_record_id(profile_id);
        let kind = credential_kind(new_kind)?;
        if let Some(replacement) = replacement {
            return Ok(CredentialUpdate::Replace(self.prepare_text_data(
                profile_id,
                &record_id,
                kind,
                Some(profile_id),
                replacement,
            )?));
        }
        if old_kind == new_kind {
            return Ok(CredentialUpdate::Keep);
        }

        let Some(stored) = load_account_data(connection, &record_id)? else {
            return Ok(CredentialUpdate::Keep);
        };
        Ok(CredentialUpdate::Replace(self.prepare_account_data(
            profile_id,
            &record_id,
            kind,
            Some(profile_id),
            &stored.value,
        )?))
    }
}

const PROVIDER_SELECT: &str = "SELECT \
    p.id, p.platform, p.kind, p.name, p.account_label, p.base_url, p.model, p.secret_kind, \
    p.profile_home, p.status, p.created_at, p.updated_at, \
    EXISTS(SELECT 1 FROM account_data d WHERE d.record_id = ('provider/' || p.id || '/credential')) \
 FROM providers p";

#[derive(Debug)]
struct ProviderRow {
    id: String,
    platform: String,
    kind: String,
    name: String,
    account_label: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    secret_kind: String,
    profile_home: Option<String>,
    status: String,
    created_at: i64,
    updated_at: i64,
    has_secret: bool,
}

fn provider_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderRow> {
    Ok(ProviderRow {
        id: row.get(0)?,
        platform: row.get(1)?,
        kind: row.get(2)?,
        name: row.get(3)?,
        account_label: row.get(4)?,
        base_url: row.get(5)?,
        model: row.get(6)?,
        secret_kind: row.get(7)?,
        profile_home: row.get(8)?,
        status: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        has_secret: row.get(12)?,
    })
}

fn fetch_provider_row(connection: &Connection, id: &str) -> Result<Option<ProviderRow>, DbError> {
    connection
        .query_row(
            &format!("{PROVIDER_SELECT} WHERE p.id = ?1"),
            [id],
            provider_row,
        )
        .optional()
        .map_err(Into::into)
}

struct PreparedAccountData {
    profile_id: String,
    record_id: String,
    kind: String,
    provider_id: Option<String>,
    value: Vec<u8>,
}

struct StoredAccountData {
    profile_id: String,
    kind: String,
    value: Vec<u8>,
}

enum CredentialUpdate {
    Keep,
    Replace(PreparedAccountData),
    Delete,
}

fn upsert_account_data(
    transaction: &Transaction<'_>,
    record: &PreparedAccountData,
    now: i64,
) -> Result<(), DbError> {
    transaction.execute(
        "INSERT INTO account_data( \
            record_id, profile_id, provider_id, record_kind, value, created_at, updated_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
         ON CONFLICT(record_id) DO UPDATE SET \
            profile_id = excluded.profile_id, provider_id = excluded.provider_id, \
            record_kind = excluded.record_kind, value = excluded.value, \
            updated_at = excluded.updated_at",
        params![
            record.record_id,
            record.profile_id,
            record.provider_id,
            record.kind,
            record.value,
            now,
        ],
    )?;
    Ok(())
}

fn load_account_data(
    connection: &Connection,
    record_id: &str,
) -> Result<Option<StoredAccountData>, DbError> {
    let row: Option<(String, String, Vec<u8>)> = connection
        .query_row(
            "SELECT profile_id, record_kind, value \
             FROM account_data WHERE record_id = ?1",
            [record_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(row.map(|(profile_id, kind, value)| StoredAccountData {
        profile_id,
        kind,
        value,
    }))
}

struct SettingsRow {
    language: String,
    theme: String,
    timezone: String,
    default_activation_mode: String,
    codex_path: Option<String>,
    claude_path: Option<String>,
    claude_desktop_path: Option<String>,
    codex_home: Option<String>,
    claude_config_dir: Option<String>,
    unify_codex_history: i64,
    unify_claude_code_history: i64,
    unify_claude_desktop_code_history: i64,
    claude_desktop_history_target: Option<String>,
}

fn settings_from_row(row: SettingsRow) -> Result<AppSettings, DbError> {
    Ok(AppSettings {
        language: row.language,
        theme: row.theme,
        timezone: row.timezone,
        default_activation_mode: activation_mode_from_db(&row.default_activation_mode)?,
        codex_path: row.codex_path,
        claude_path: row.claude_path,
        claude_desktop_path: row.claude_desktop_path,
        codex_home: row.codex_home,
        claude_config_dir: row.claude_config_dir,
        unify_codex_history: bool_from_db(row.unify_codex_history, "settings.unify_codex_history")?,
        unify_claude_code_history: bool_from_db(
            row.unify_claude_code_history,
            "settings.unify_claude_code_history",
        )?,
        unify_claude_desktop_code_history: bool_from_db(
            row.unify_claude_desktop_code_history,
            "settings.unify_claude_desktop_code_history",
        )?,
        claude_desktop_history_target: row.claude_desktop_history_target,
    })
}

/// Locator for backend-only account data.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SensitiveRecordKey<'a> {
    pub profile_id: &'a str,
    pub record_id: &'a str,
    pub kind: &'a str,
    pub provider_id: Option<&'a str>,
}

pub(crate) struct SensitiveBytes(SecretSlice<u8>);

impl SensitiveBytes {
    pub(crate) fn expose(&self) -> &[u8] {
        self.0.expose_secret()
    }
}

impl fmt::Debug for SensitiveBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveBytes([REDACTED])")
    }
}

impl Repository {
    /// Backend-only credential accessor. The wrapper is redacted in debug output
    /// and cannot be serialized into an ordinary profile response.
    pub(crate) fn load_provider_secret(
        &self,
        profile_id: &str,
    ) -> Result<Option<SecretString>, DbError> {
        let connection = self.lock()?;
        let Some(record) = load_account_data(&connection, &credential_record_id(profile_id))?
        else {
            return Ok(None);
        };
        String::from_utf8(record.value)
            .map(SecretString::from)
            .map(Some)
            .map_err(|_| DbError::InvalidCredentialEncoding {
                record_id: credential_record_id(profile_id),
            })
    }

    /// Store account data such as an auth bundle or global-switch baseline.
    pub(crate) fn store_sensitive_record(
        &self,
        key: SensitiveRecordKey<'_>,
        value: &[u8],
    ) -> Result<(), DbError> {
        let prepared = self.prepare_account_data(
            key.profile_id,
            key.record_id,
            key.kind,
            key.provider_id,
            value,
        )?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_account_data(&transaction, &prepared, now_millis())?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn load_sensitive_record(
        &self,
        key: SensitiveRecordKey<'_>,
    ) -> Result<Option<SensitiveBytes>, DbError> {
        validate_sensitive_locator(key.profile_id, key.record_id, key.kind)?;
        let connection = self.lock()?;
        let Some(record) = load_account_data(&connection, key.record_id)? else {
            return Ok(None);
        };
        if record.profile_id != key.profile_id || record.kind != key.kind {
            return Err(DbError::SensitiveRecordContextMismatch {
                record_id: key.record_id.to_owned(),
            });
        }
        Ok(Some(SensitiveBytes(record.value.into())))
    }

    pub(crate) fn delete_sensitive_record(&self, record_id: &str) -> Result<bool, DbError> {
        if record_id.is_empty() {
            return Err(DbError::InvalidInput("secret record_id must not be empty"));
        }
        let connection = self.lock()?;
        Ok(connection.execute("DELETE FROM account_data WHERE record_id = ?1", [record_id])? == 1)
    }

    /// Finish global deactivation as one database state transition. The
    /// external config and credential rollback happens before this call; if
    /// either database change fails, both the active binding and recovery
    /// baseline remain available for a retry.
    pub(crate) fn clear_global_profile_and_delete_sensitive_record(
        &self,
        platform: Platform,
        record_id: &str,
    ) -> Result<(), DbError> {
        if record_id.is_empty() {
            return Err(DbError::InvalidInput("secret record_id must not be empty"));
        }

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding_changed = transaction.execute(
            "UPDATE platform_bindings SET global_profile_id = NULL, updated_at = ?1 WHERE platform = ?2",
            params![now_millis(), platform.as_str()],
        )?;
        if binding_changed != 1 {
            return Err(DbError::NotFound {
                entity: "platform_binding",
                id: platform.as_str().to_owned(),
            });
        }

        let baseline_deleted =
            transaction.execute("DELETE FROM account_data WHERE record_id = ?1", [record_id])?;
        if baseline_deleted != 1 {
            return Err(DbError::NotFound {
                entity: "sensitive_record",
                id: record_id.to_owned(),
            });
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn update_provider_runtime_state(
        &self,
        id: &str,
        status: ProfileStatus,
        profile_home: Option<&str>,
    ) -> Result<ProviderProfile, DbError> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE providers SET status = ?1, profile_home = ?2, updated_at = ?3 WHERE id = ?4",
            params![profile_status_to_db(status), profile_home, now_millis(), id,],
        )?;
        if changed != 1 {
            return Err(DbError::NotFound {
                entity: "provider",
                id: id.to_owned(),
            });
        }
        drop(connection);
        self.get_provider(id)?.ok_or_else(|| DbError::NotFound {
            entity: "provider",
            id: id.to_owned(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct UsageRecordInput {
    /// Stable parser-provided ID or content fingerprint. `(platform, event_id)`
    /// is the idempotency key.
    pub event_id: String,
    pub platform: Platform,
    pub occurred_at: i64,
    pub tokens: TokenBreakdown,
    pub request_count: u64,
}

#[derive(Clone, Debug)]
pub struct UsageScanSummary {
    pub platform: Platform,
    pub diagnostics: UsageDiagnostics,
}

impl Repository {
    /// Replaces one platform's local usage index with a complete scan snapshot.
    pub fn replace_usage_snapshot(
        &self,
        platform: Platform,
        records: &[UsageRecordInput],
    ) -> Result<usize, DbError> {
        for record in records {
            if record.platform != platform {
                return Err(DbError::InvalidInput(
                    "usage snapshot contains another platform",
                ));
            }
            validate_usage(record)?;
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM usage WHERE platform = ?1", [platform.as_str()])?;
        let mut statement = transaction.prepare(
            "INSERT OR REPLACE INTO usage( \
                platform, event_id, occurred_at, uncached_input, cache_read, \
                cache_write, output, reasoning_output, request_count, created_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let now = now_millis();
        for usage in records {
            statement.execute(params![
                usage.platform.as_str(),
                usage.event_id,
                usage.occurred_at,
                count_to_db(usage.tokens.uncached_input, "uncached_input")?,
                count_to_db(usage.tokens.cache_read, "cache_read")?,
                count_to_db(usage.tokens.cache_write, "cache_write")?,
                count_to_db(usage.tokens.output, "output")?,
                count_to_db(usage.tokens.reasoning_output, "reasoning_output")?,
                count_to_db(usage.request_count, "request_count")?,
                now,
            ])?;
        }
        drop(statement);
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM usage WHERE platform = ?1",
            [platform.as_str()],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        usize::try_from(count).map_err(|_| DbError::CorruptInteger("usage count"))
    }

    /// Return events in the half-open UTC millisecond range
    /// `[start_at, end_at)`. Date/timezone grouping stays in the usage service.
    pub fn usage_rows(
        &self,
        platform: Platform,
        start_at: i64,
        end_at: i64,
    ) -> Result<Vec<UsageRecordInput>, DbError> {
        if end_at < start_at {
            return Err(DbError::InvalidInput("usage end must not precede start"));
        }
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT event_id, occurred_at, uncached_input, cache_read, cache_write, \
                    output, reasoning_output, request_count \
             FROM usage \
             WHERE platform = ?1 AND occurred_at >= ?2 AND occurred_at < ?3 \
             ORDER BY occurred_at, event_id",
        )?;
        let rows = statement
            .query_map(params![platform.as_str(), start_at, end_at], |row| {
                Ok(UsageRow {
                    event_id: row.get(0)?,
                    occurred_at: row.get(1)?,
                    uncached_input: row.get(2)?,
                    cache_read: row.get(3)?,
                    cache_write: row.get(4)?,
                    output: row.get(5)?,
                    reasoning_output: row.get(6)?,
                    request_count: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| usage_from_row(platform, row))
            .collect()
    }

    pub fn load_usage_scan_summary(
        &self,
        platform: Platform,
    ) -> Result<Option<UsageScanSummary>, DbError> {
        let connection = self.lock()?;
        let row = connection
            .query_row(
                "SELECT files_scanned, malformed_records, duplicate_records, coverage_start, \
                        coverage_end, last_scanned_at, is_partial \
                 FROM usage_scan_summary WHERE platform = ?1",
                [platform.as_str()],
                usage_scan_summary_row,
            )
            .optional()?;
        row.map(|row| usage_scan_summary_from_row(platform, row))
            .transpose()
    }

    pub fn save_usage_scan_summary(
        &self,
        summary: &UsageScanSummary,
    ) -> Result<UsageScanSummary, DbError> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO usage_scan_summary( \
                platform, files_scanned, malformed_records, duplicate_records, coverage_start, \
                coverage_end, last_scanned_at, is_partial \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(platform) DO UPDATE SET \
                files_scanned = excluded.files_scanned, malformed_records = excluded.malformed_records, \
                duplicate_records = excluded.duplicate_records, coverage_start = excluded.coverage_start, \
                coverage_end = excluded.coverage_end, last_scanned_at = excluded.last_scanned_at, \
                is_partial = excluded.is_partial",
            params![
                summary.platform.as_str(),
                count_to_db(summary.diagnostics.files_scanned, "files_scanned")?,
                count_to_db(summary.diagnostics.malformed_records, "malformed_records")?,
                count_to_db(summary.diagnostics.duplicate_records, "duplicate_records")?,
                summary.diagnostics.coverage_start,
                summary.diagnostics.coverage_end,
                summary.diagnostics.last_scanned_at,
                bool_to_db(summary.diagnostics.is_partial),
            ],
        )?;
        drop(connection);
        self.load_usage_scan_summary(summary.platform)?
            .ok_or_else(|| DbError::NotFound {
                entity: "usage_scan_summary",
                id: summary.platform.as_str().to_owned(),
            })
    }
}

struct UsageRow {
    event_id: String,
    occurred_at: i64,
    uncached_input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    reasoning_output: i64,
    request_count: i64,
}

fn usage_from_row(platform: Platform, row: UsageRow) -> Result<UsageRecordInput, DbError> {
    Ok(UsageRecordInput {
        event_id: row.event_id,
        platform,
        occurred_at: row.occurred_at,
        tokens: TokenBreakdown {
            uncached_input: count_from_db(row.uncached_input, "uncached_input")?,
            cache_read: count_from_db(row.cache_read, "cache_read")?,
            cache_write: count_from_db(row.cache_write, "cache_write")?,
            output: count_from_db(row.output, "output")?,
            reasoning_output: count_from_db(row.reasoning_output, "reasoning_output")?,
        },
        request_count: count_from_db(row.request_count, "request_count")?,
    })
}

struct UsageScanSummaryRow {
    files_scanned: i64,
    malformed_records: i64,
    duplicate_records: i64,
    coverage_start: Option<i64>,
    coverage_end: Option<i64>,
    last_scanned_at: Option<i64>,
    is_partial: i64,
}

fn usage_scan_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageScanSummaryRow> {
    Ok(UsageScanSummaryRow {
        files_scanned: row.get(0)?,
        malformed_records: row.get(1)?,
        duplicate_records: row.get(2)?,
        coverage_start: row.get(3)?,
        coverage_end: row.get(4)?,
        last_scanned_at: row.get(5)?,
        is_partial: row.get(6)?,
    })
}

fn usage_scan_summary_from_row(
    platform: Platform,
    row: UsageScanSummaryRow,
) -> Result<UsageScanSummary, DbError> {
    Ok(UsageScanSummary {
        platform,
        diagnostics: UsageDiagnostics {
            files_scanned: count_from_db(row.files_scanned, "files_scanned")?,
            malformed_records: count_from_db(row.malformed_records, "malformed_records")?,
            duplicate_records: count_from_db(row.duplicate_records, "duplicate_records")?,
            coverage_start: row.coverage_start,
            coverage_end: row.coverage_end,
            last_scanned_at: row.last_scanned_at,
            is_partial: bool_from_db(row.is_partial, "is_partial")?,
        },
    })
}

fn configure_connection(connection: &Connection) -> Result<(), DbError> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn ensure_settings_row(connection: &Connection) -> Result<(), DbError> {
    connection.execute(
        "INSERT OR IGNORE INTO settings( \
            singleton_id, language, theme, timezone, default_activation_mode, updated_at \
         ) VALUES (1, ?1, ?2, ?3, 'managed_launch', ?4)",
        params![
            DEFAULT_LANGUAGE,
            DEFAULT_THEME,
            DEFAULT_TIMEZONE,
            now_millis(),
        ],
    )?;
    Ok(())
}

pub(super) fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn validate_provider_name(name: &str) -> Result<(), DbError> {
    if name.trim().is_empty() {
        return Err(DbError::InvalidInput("provider name must not be empty"));
    }
    Ok(())
}

fn validate_secret_input(kind: SecretKind, secret: Option<&str>) -> Result<(), DbError> {
    if kind == SecretKind::None && secret.is_some() {
        return Err(DbError::InvalidInput(
            "a provider with secret kind 'none' cannot contain a secret",
        ));
    }
    if secret.is_some_and(str::is_empty) {
        return Err(DbError::InvalidInput("provider secret must not be empty"));
    }
    Ok(())
}

fn validate_sensitive_locator(
    profile_id: &str,
    record_id: &str,
    kind: &str,
) -> Result<(), DbError> {
    if profile_id.is_empty() {
        return Err(DbError::InvalidInput("secret profile_id must not be empty"));
    }
    if record_id.is_empty() {
        return Err(DbError::InvalidInput("secret record_id must not be empty"));
    }
    if kind.is_empty() {
        return Err(DbError::InvalidInput("secret kind must not be empty"));
    }
    Ok(())
}

fn validate_usage(usage: &UsageRecordInput) -> Result<(), DbError> {
    if usage.event_id.is_empty() {
        return Err(DbError::InvalidInput("usage event_id must not be empty"));
    }
    for (name, value) in [
        ("uncached_input", usage.tokens.uncached_input),
        ("cache_read", usage.tokens.cache_read),
        ("cache_write", usage.tokens.cache_write),
        ("output", usage.tokens.output),
        ("reasoning_output", usage.tokens.reasoning_output),
        ("request_count", usage.request_count),
    ] {
        count_to_db(value, name)?;
    }
    Ok(())
}

fn initial_status(
    provider_kind: ProviderKind,
    secret_kind: SecretKind,
    secret: Option<&str>,
) -> ProfileStatus {
    if provider_kind == ProviderKind::OfficialSubscription
        || (secret_kind != SecretKind::None && secret.is_none())
    {
        ProfileStatus::NeedsLogin
    } else {
        ProfileStatus::Ready
    }
}

fn credential_record_id(profile_id: &str) -> String {
    format!("provider/{profile_id}/{CREDENTIAL_RECORD_NAME}")
}

fn fetch_platform_binding(
    connection: &Connection,
    platform: Platform,
) -> Result<Option<PlatformBinding>, DbError> {
    let row: Option<(Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT global_profile_id, last_managed_profile_id \
             FROM platform_bindings WHERE platform = ?1",
            [platform.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row.map(
        |(global_profile_id, last_managed_profile_id)| PlatformBinding {
            platform,
            global_profile_id,
            last_managed_profile_id,
        },
    ))
}

fn count_to_db(value: u64, field: &'static str) -> Result<i64, DbError> {
    i64::try_from(value).map_err(|_| DbError::IntegerOverflow(field))
}

fn count_from_db(value: i64, field: &'static str) -> Result<u64, DbError> {
    u64::try_from(value).map_err(|_| DbError::CorruptInteger(field))
}

const fn bool_to_db(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn bool_from_db(value: i64, field: &'static str) -> Result<bool, DbError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DbError::CorruptInteger(field)),
    }
}

const fn provider_kind_to_db(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OfficialSubscription => "official_subscription",
        ProviderKind::OfficialApi => "official_api",
        ProviderKind::ThirdParty => "third_party",
    }
}

fn provider_kind_from_db(value: &str) -> Result<ProviderKind, DbError> {
    match value {
        "official_subscription" => Ok(ProviderKind::OfficialSubscription),
        "official_api" => Ok(ProviderKind::OfficialApi),
        "third_party" => Ok(ProviderKind::ThirdParty),
        _ => Err(DbError::InvalidEnum {
            field: "provider.kind",
            value: value.to_owned(),
        }),
    }
}

const fn activation_mode_to_db(mode: ActivationMode) -> &'static str {
    match mode {
        ActivationMode::ManagedLaunch => "managed_launch",
        ActivationMode::GlobalCredential => "global_credential",
    }
}

fn activation_mode_from_db(value: &str) -> Result<ActivationMode, DbError> {
    match value {
        "managed_launch" => Ok(ActivationMode::ManagedLaunch),
        "global_credential" => Ok(ActivationMode::GlobalCredential),
        _ => Err(DbError::InvalidEnum {
            field: "activation_mode",
            value: value.to_owned(),
        }),
    }
}

const fn secret_kind_to_db(kind: SecretKind) -> &'static str {
    match kind {
        SecretKind::None => "none",
        SecretKind::ApiKey => "api_key",
        SecretKind::BearerToken => "bearer_token",
    }
}

fn secret_kind_from_db(value: &str) -> Result<SecretKind, DbError> {
    match value {
        "none" => Ok(SecretKind::None),
        "api_key" => Ok(SecretKind::ApiKey),
        "bearer_token" => Ok(SecretKind::BearerToken),
        _ => Err(DbError::InvalidEnum {
            field: "provider.secret_kind",
            value: value.to_owned(),
        }),
    }
}

fn credential_kind(kind: SecretKind) -> Result<&'static str, DbError> {
    match kind {
        SecretKind::ApiKey => Ok("credential.api_key"),
        SecretKind::BearerToken => Ok("credential.bearer_token"),
        SecretKind::None => Err(DbError::InvalidInput(
            "secret kind 'none' has no credential record kind",
        )),
    }
}

const fn profile_status_to_db(status: ProfileStatus) -> &'static str {
    match status {
        ProfileStatus::Ready => "ready",
        ProfileStatus::NeedsLogin => "needs_login",
    }
}

fn profile_status_from_db(value: &str) -> Result<ProfileStatus, DbError> {
    match value {
        "ready" => Ok(ProfileStatus::Ready),
        "needs_login" => Ok(ProfileStatus::NeedsLogin),
        _ => Err(DbError::InvalidEnum {
            field: "provider.status",
            value: value.to_owned(),
        }),
    }
}

fn platform_from_db(value: &str) -> Result<Platform, DbError> {
    match value {
        "codex" => Ok(Platform::Codex),
        "claude_code" => Ok(Platform::ClaudeCode),
        "claude_desktop" => Ok(Platform::ClaudeDesktop),
        _ => Err(DbError::InvalidEnum {
            field: "platform",
            value: value.to_owned(),
        }),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    DatabaseTooNew { found: i64, supported: i64 },
    #[error("{entity} '{id}' was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("provider '{id}' is active for platform '{platform}'")]
    ProviderActive { id: String, platform: String },
    #[error("provider '{profile_id}' belongs to '{actual}', not '{expected}'")]
    PlatformMismatch {
        profile_id: String,
        expected: String,
        actual: String,
    },
    #[error("invalid persisted enum in {field}: '{value}'")]
    InvalidEnum { field: &'static str, value: String },
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("integer '{0}' exceeds SQLite's signed range")]
    IntegerOverflow(&'static str),
    #[error("database contains invalid integer '{0}'")]
    CorruptInteger(&'static str),
    #[error("account data '{record_id}' does not match its requested context")]
    SensitiveRecordContextMismatch { record_id: String },
    #[error("credential '{record_id}' is not valid UTF-8")]
    InvalidCredentialEncoding { record_id: String },
    #[error("database mutex is poisoned")]
    LockPoisoned,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use secrecy::ExposeSecret;

    use super::*;

    fn repository() -> Repository {
        Repository::in_memory().unwrap()
    }

    fn create_provider(
        repository: &Repository,
        platform: Platform,
        label: &str,
        secret: &str,
    ) -> ProviderProfile {
        repository
            .create_provider(&CreateProviderRequest {
                platform,
                kind: ProviderKind::ThirdParty,
                name: format!("{label} provider"),
                account_label: Some(label.to_owned()),
                base_url: Some("https://api.example.test".to_owned()),
                model: Some("model-a".to_owned()),
                secret_kind: SecretKind::ApiKey,
                secret: Some(secret.to_owned()),
                official_credential: None,
            })
            .unwrap()
    }

    #[test]
    fn migration_creates_the_minimal_v1_schema_and_defaults() {
        let repository = repository();
        let connection = repository.lock().unwrap();
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();
        for required in [
            "providers",
            "platform_bindings",
            "settings",
            "account_data",
            "usage",
            "usage_scan_summary",
            "schema_migrations",
        ] {
            assert!(tables.contains(required), "missing {required}");
        }
        assert!(!tables.contains("active_bindings"));
        assert!(!tables.contains("managed_fields"));
        assert!(!tables.contains("switch_operations"));
        drop(statement);
        drop(connection);

        let settings = repository.load_settings().unwrap();
        assert_eq!(settings.language, "system");
        assert_eq!(settings.theme, "auto");
        assert!(!settings.unify_codex_history);
        assert!(!settings.unify_claude_code_history);
        assert!(!settings.unify_claude_desktop_code_history);
        for platform in Platform::ALL {
            assert_eq!(
                repository.get_platform_binding(platform).unwrap(),
                PlatformBinding {
                    platform,
                    global_profile_id: None,
                    last_managed_profile_id: None,
                }
            );
        }
    }

    #[test]
    fn credentials_are_stored_and_not_returned_in_profiles() {
        let repository = repository();
        let profile = create_provider(&repository, Platform::Codex, "work", "plain-secret");
        assert!(profile.has_secret);
        assert_eq!(
            repository
                .load_provider_secret(&profile.id)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "plain-secret"
        );
        let connection = repository.lock().unwrap();
        let stored: Vec<u8> = connection
            .query_row("SELECT value FROM account_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, b"plain-secret");
    }

    #[test]
    fn official_subscription_starts_in_needs_login_state() {
        let repository = repository();
        let profile = repository
            .create_provider(&CreateProviderRequest {
                platform: Platform::Codex,
                kind: ProviderKind::OfficialSubscription,
                name: "Personal".to_owned(),
                account_label: None,
                base_url: None,
                model: None,
                secret_kind: SecretKind::None,
                secret: None,
                official_credential: None,
            })
            .unwrap();

        assert_eq!(profile.status, ProfileStatus::NeedsLogin);
        assert!(!profile.has_secret);
    }

    #[test]
    fn global_and_managed_bindings_are_independent() {
        let repository = repository();
        let global = create_provider(&repository, Platform::Codex, "global", "one");
        let managed = create_provider(&repository, Platform::Codex, "managed", "two");
        repository
            .set_global_profile(Platform::Codex, Some(&global.id))
            .unwrap();
        let binding = repository
            .set_last_managed_profile(Platform::Codex, Some(&managed.id))
            .unwrap();
        assert_eq!(
            binding.global_profile_id.as_deref(),
            Some(global.id.as_str())
        );
        assert_eq!(
            binding.last_managed_profile_id.as_deref(),
            Some(managed.id.as_str())
        );

        repository
            .delete_provider(&DeleteProviderRequest { id: managed.id })
            .unwrap();
        assert_eq!(
            repository
                .get_platform_binding(Platform::Codex)
                .unwrap()
                .last_managed_profile_id,
            None
        );
        assert!(matches!(
            repository.delete_provider(&DeleteProviderRequest { id: global.id }),
            Err(DbError::ProviderActive { .. })
        ));
    }

    #[test]
    fn deleting_desktop_history_target_disables_automatic_sync() {
        let repository = repository();
        let profile = create_provider(&repository, Platform::ClaudeDesktop, "desktop", "secret");
        let mut settings = repository.load_settings().unwrap();
        settings.unify_claude_desktop_code_history = true;
        settings.claude_desktop_history_target =
            Some(format!("profile:{}:account:organization", profile.id));
        repository.save_settings(&settings).unwrap();

        repository
            .delete_provider(&DeleteProviderRequest { id: profile.id })
            .unwrap();

        let settings = repository.load_settings().unwrap();
        assert!(!settings.unify_claude_desktop_code_history);
        assert_eq!(settings.claude_desktop_history_target, None);
    }

    #[test]
    fn global_deactivation_binding_and_baseline_are_atomic() {
        let repository = repository();
        let global = create_provider(&repository, Platform::Codex, "global", "one");
        repository
            .set_global_profile(Platform::Codex, Some(&global.id))
            .unwrap();
        let key = SensitiveRecordKey {
            profile_id: "codex",
            record_id: "global/codex/baseline",
            kind: "global.baseline.v1",
            provider_id: None,
        };
        repository
            .store_sensitive_record(key, b"baseline payload")
            .unwrap();

        assert!(matches!(
            repository.clear_global_profile_and_delete_sensitive_record(
                Platform::Codex,
                "global/codex/missing-baseline",
            ),
            Err(DbError::NotFound {
                entity: "sensitive_record",
                ..
            })
        ));
        assert_eq!(
            repository
                .get_platform_binding(Platform::Codex)
                .unwrap()
                .global_profile_id
                .as_deref(),
            Some(global.id.as_str())
        );
        assert!(repository.load_sensitive_record(key).unwrap().is_some());

        repository
            .clear_global_profile_and_delete_sensitive_record(Platform::Codex, key.record_id)
            .unwrap();
        assert_eq!(
            repository
                .get_platform_binding(Platform::Codex)
                .unwrap()
                .global_profile_id,
            None
        );
        assert!(repository.load_sensitive_record(key).unwrap().is_none());
    }

    #[test]
    fn binding_rejects_cross_platform_provider() {
        let repository = repository();
        let profile = create_provider(&repository, Platform::ClaudeCode, "claude", "secret");
        assert!(matches!(
            repository.set_global_profile(Platform::Codex, Some(&profile.id)),
            Err(DbError::PlatformMismatch { .. })
        ));
    }

    #[test]
    fn sensitive_records_validate_their_locator() {
        let repository = repository();
        let key = SensitiveRecordKey {
            profile_id: "codex",
            record_id: "global/codex/baseline",
            kind: "global.baseline.v1",
            provider_id: None,
        };
        repository
            .store_sensitive_record(key, b"baseline-secret")
            .unwrap();
        assert_eq!(
            repository
                .load_sensitive_record(key)
                .unwrap()
                .unwrap()
                .expose(),
            b"baseline-secret"
        );
        assert!(matches!(
            repository.load_sensitive_record(SensitiveRecordKey {
                kind: "wrong-kind",
                ..key
            }),
            Err(DbError::SensitiveRecordContextMismatch { .. })
        ));
        assert!(
            repository
                .delete_sensitive_record("global/codex/baseline")
                .unwrap()
        );
    }

    #[test]
    fn complete_usage_snapshot_replaces_previous_values() {
        let repository = repository();
        let mut usage = UsageRecordInput {
            event_id: "stable-event".to_owned(),
            platform: Platform::ClaudeCode,
            occurred_at: 1_234,
            tokens: TokenBreakdown {
                output: 1,
                ..TokenBreakdown::default()
            },
            request_count: 1,
        };
        repository
            .replace_usage_snapshot(Platform::ClaudeCode, &[usage.clone()])
            .unwrap();
        usage.tokens.output = 100;
        repository
            .replace_usage_snapshot(Platform::ClaudeCode, &[usage])
            .unwrap();

        let rows = repository
            .usage_rows(Platform::ClaudeCode, 1_000, 2_000)
            .unwrap();
        assert_eq!(rows[0].tokens.output, 100);
    }
}
