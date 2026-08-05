//! Claude Desktop official-account snapshot capture and restoration.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::activation::{ConfigFormat, OwnedPath, PatchEngine, PatchOperation};

use super::claude_desktop_safe_storage;

const ACCOUNT_SNAPSHOT_VERSION: u32 = 1;
const CONFIG_STORE_FILE: &str = "config.json";
const COOKIES_FILE: &str = "Cookies";
const ACCOUNT_UUID_KEY: &str = "lastKnownAccountUuid";
const OAUTH_CACHE_KEY: &str = "oauth:tokenCache";
const OAUTH_CACHE_V2_KEY: &str = "oauth:tokenCacheV2";
const MAX_COOKIE_BLOB_BYTES: usize = 64 * 1024;
const SUPPORTED_COOKIE_DATABASE_VERSION: u32 = 24;
const AUTH_COOKIE_NAMES: [&str; 5] = [
    "sessionKey",
    "sessionKeyLC",
    "lastActiveOrg",
    "routingHint",
    "__Host-ant_trusted_device",
];

#[derive(Deserialize, Serialize, Zeroize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountData {
    version: u32,
    account_uuid: String,
    oauth_cache: Option<String>,
    oauth_cache_v2: Option<String>,
    cookies: Vec<CookieRecord>,
}

impl AccountData {
    pub(super) fn label(&self) -> String {
        format!("Claude Desktop {}", &self.account_uuid[..8])
    }
}

#[derive(Deserialize, Serialize, Zeroize)]
#[serde(deny_unknown_fields)]
struct CookieRecord {
    creation_utc: i64,
    host_key: String,
    top_frame_site_key: String,
    name: String,
    value: String,
    path: String,
    expires_utc: i64,
    is_secure: i64,
    is_httponly: i64,
    last_access_utc: i64,
    has_expires: i64,
    is_persistent: i64,
    priority: i64,
    samesite: i64,
    source_scheme: i64,
    source_port: i64,
    last_update_utc: i64,
    source_type: i64,
    has_cross_site_ancestor: i64,
}

pub(super) fn has_account(config: &Value) -> bool {
    config
        .get(OAUTH_CACHE_KEY)
        .and_then(Value::as_str)
        .is_some()
        || config
            .get(OAUTH_CACHE_V2_KEY)
            .and_then(Value::as_str)
            .is_some()
}

pub(super) fn capture(
    root: &Path,
    config: &Value,
) -> Result<(AccountData, Option<String>), String> {
    let account_uuid = config
        .get(ACCOUNT_UUID_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| "Claude Desktop has no active account".to_string())?;
    let account_uuid = uuid::Uuid::parse_str(account_uuid)
        .map_err(|_| "Claude Desktop stored an invalid account identity".to_string())?
        .to_string();
    let oauth_cache = decrypt_optional_cache(root, config, OAUTH_CACHE_KEY)?;
    let oauth_cache_v2 = decrypt_optional_cache(root, config, OAUTH_CACHE_V2_KEY)?;
    if oauth_cache.is_none() && oauth_cache_v2.is_none() {
        return Err("Claude Desktop has no persisted OAuth token cache".into());
    }
    let (cookies, warning) = capture_cookies(root, &root.join(COOKIES_FILE))?;
    Ok((
        AccountData {
            version: ACCOUNT_SNAPSHOT_VERSION,
            account_uuid,
            oauth_cache,
            oauth_cache_v2,
            cookies,
        },
        warning,
    ))
}

pub(super) fn restore(root: &Path, data: &AccountData) -> Result<(), String> {
    if data.version != ACCOUNT_SNAPSHOT_VERSION {
        return Err(format!(
            "Claude Desktop account snapshot version {} is unsupported",
            data.version
        ));
    }
    let account_uuid = uuid::Uuid::parse_str(&data.account_uuid)
        .map_err(|_| "saved Claude Desktop account identity is invalid".to_string())?
        .to_string();
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    validate_cookie_database(&root.join(COOKIES_FILE))?;
    let operations = vec![
        PatchOperation::set(path(ACCOUNT_UUID_KEY)?, Value::String(account_uuid)),
        encrypted_cache_operation(root, OAUTH_CACHE_KEY, data.oauth_cache.as_deref())?,
        encrypted_cache_operation(root, OAUTH_CACHE_V2_KEY, data.oauth_cache_v2.as_deref())?,
    ];
    let prepared =
        PatchEngine::prepare_file(root.join(CONFIG_STORE_FILE), ConfigFormat::Json, operations)
            .map_err(|error| error.to_string())?;
    PatchEngine::commit(prepared).map_err(|error| error.to_string())?;
    restore_cookies(root, &root.join(COOKIES_FILE), &data.cookies)
}

pub(super) fn clear(root: &Path) -> Result<(), String> {
    let config_path = root.join(CONFIG_STORE_FILE);
    if config_path.exists() {
        let operations = [ACCOUNT_UUID_KEY, OAUTH_CACHE_KEY, OAUTH_CACHE_V2_KEY]
            .into_iter()
            .map(|key| path(key).map(PatchOperation::remove))
            .collect::<Result<Vec<_>, _>>()?;
        let prepared = PatchEngine::prepare_file(config_path, ConfigFormat::Json, operations)
            .map_err(|error| error.to_string())?;
        PatchEngine::commit(prepared).map_err(|error| error.to_string())?;
    }
    clear_auth_cookies(&root.join(COOKIES_FILE))
}

fn decrypt_optional_cache(
    root: &Path,
    config: &Value,
    key: &str,
) -> Result<Option<String>, String> {
    config
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| format!("Claude Desktop {key} must be a string"))
                .and_then(|value| claude_desktop_safe_storage::decrypt_base64(root, value))
                .map(|value| value.to_string())
        })
        .transpose()
}

fn encrypted_cache_operation(
    root: &Path,
    key: &str,
    value: Option<&str>,
) -> Result<PatchOperation, String> {
    match value {
        Some(value) => Ok(PatchOperation::set(
            path(key)?,
            Value::String(claude_desktop_safe_storage::encrypt_base64(root, value)?.to_string()),
        )),
        None => Ok(PatchOperation::remove(path(key)?)),
    }
}

fn path(key: &str) -> Result<OwnedPath, String> {
    OwnedPath::from_segments([key]).map_err(|error| error.to_string())
}

fn capture_cookies(
    root: &Path,
    path: &Path,
) -> Result<(Vec<CookieRecord>, Option<String>), String> {
    capture_cookies_with_codec(path, |ciphertext| {
        claude_desktop_safe_storage::decrypt_bytes(root, ciphertext).map(|value| value.to_vec())
    })
}

fn capture_cookies_with_codec<F>(
    path: &Path,
    decrypt: F,
) -> Result<(Vec<CookieRecord>, Option<String>), String>
where
    F: Fn(&[u8]) -> Result<Vec<u8>, String>,
{
    if !path.exists() {
        return Ok((Vec::new(), None));
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to open Claude Desktop cookies: {error}"))?;
    let (database_version, warning) = cookie_database_version_for_capture(&connection)?;
    let mut statement = connection
        .prepare(
            "SELECT creation_utc, host_key, top_frame_site_key, name, value, encrypted_value, \
                    path, expires_utc, is_secure, is_httponly, last_access_utc, has_expires, \
                    is_persistent, priority, samesite, source_scheme, source_port, \
                    last_update_utc, source_type, has_cross_site_ancestor \
             FROM cookies \
             WHERE (host_key = 'claude.ai' OR host_key = '.claude.ai') \
               AND name IN (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(|error| format!("Claude Desktop cookie schema is unsupported: {error}"))?;
    let rows = statement
        .query_map(
            params![
                AUTH_COOKIE_NAMES[0],
                AUTH_COOKIE_NAMES[1],
                AUTH_COOKIE_NAMES[2],
                AUTH_COOKIE_NAMES[3],
                AUTH_COOKIE_NAMES[4],
            ],
            |row| {
                Ok((
                    CookieRecord {
                        creation_utc: row.get(0)?,
                        host_key: row.get(1)?,
                        top_frame_site_key: row.get(2)?,
                        name: row.get(3)?,
                        value: row.get(4)?,
                        path: row.get(6)?,
                        expires_utc: row.get(7)?,
                        is_secure: row.get(8)?,
                        is_httponly: row.get(9)?,
                        last_access_utc: row.get(10)?,
                        has_expires: row.get(11)?,
                        is_persistent: row.get(12)?,
                        priority: row.get(13)?,
                        samesite: row.get(14)?,
                        source_scheme: row.get(15)?,
                        source_port: row.get(16)?,
                        last_update_utc: row.get(17)?,
                        source_type: row.get(18)?,
                        has_cross_site_ancestor: row.get(19)?,
                    },
                    row.get::<_, Vec<u8>>(5)?,
                ))
            },
        )
        .map_err(|error| format!("failed to read Claude Desktop cookies: {error}"))?;
    let mut records = Vec::new();
    for row in rows {
        let (mut record, encrypted_value) =
            row.map_err(|error| format!("failed to read Claude Desktop cookie: {error}"))?;
        if record.value.len() > MAX_COOKIE_BLOB_BYTES
            || encrypted_value.len() > MAX_COOKIE_BLOB_BYTES
        {
            return Err("Claude Desktop authentication cookie is unexpectedly large".into());
        }
        if !record.value.is_empty() && !encrypted_value.is_empty() {
            return Err("Claude Desktop authentication cookie has conflicting values".into());
        }
        if !encrypted_value.is_empty() {
            let plaintext = Zeroizing::new(decrypt(&encrypted_value)?);
            let value = cookie_value(&record.host_key, database_version, &plaintext)?;
            record.value = String::from_utf8(value.to_vec()).map_err(|_| {
                "Claude Desktop authentication cookie is not valid UTF-8".to_string()
            })?;
        }
        records.push(record);
    }
    Ok((records, warning))
}

fn restore_cookies(root: &Path, path: &Path, records: &[CookieRecord]) -> Result<(), String> {
    restore_cookies_with_codec(path, records, |plaintext| {
        claude_desktop_safe_storage::encrypt_bytes(root, plaintext).map(|value| value.to_vec())
    })
}

fn restore_cookies_with_codec<F>(
    path: &Path,
    records: &[CookieRecord],
    encrypt: F,
) -> Result<(), String>
where
    F: Fn(&[u8]) -> Result<Vec<u8>, String>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut connection = Connection::open(path)
        .map_err(|error| format!("failed to open Claude Desktop cookies: {error}"))?;
    ensure_cookie_schema(&connection)?;
    let database_version = cookie_database_version_for_restore(&connection)?;
    let transaction = connection
        .transaction()
        .map_err(|error| format!("failed to update Claude Desktop cookies: {error}"))?;
    delete_auth_cookies(&transaction)?;
    for record in records {
        if record.value.len() > MAX_COOKIE_BLOB_BYTES {
            return Err("Claude Desktop authentication cookie is unexpectedly large".into());
        }
        let plaintext = cookie_plaintext(&record.host_key, database_version, &record.value)?;
        let encrypted_value = Zeroizing::new(encrypt(&plaintext)?);
        transaction
            .execute(
                "INSERT OR REPLACE INTO cookies( \
                    creation_utc, host_key, top_frame_site_key, name, value, encrypted_value, \
                    path, expires_utc, is_secure, is_httponly, last_access_utc, has_expires, \
                    is_persistent, priority, samesite, source_scheme, source_port, \
                    last_update_utc, source_type, has_cross_site_ancestor \
                 ) VALUES ( \
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                    ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20 \
                 )",
                params![
                    record.creation_utc,
                    record.host_key,
                    record.top_frame_site_key,
                    record.name,
                    "",
                    encrypted_value.as_slice(),
                    record.path,
                    record.expires_utc,
                    record.is_secure,
                    record.is_httponly,
                    record.last_access_utc,
                    record.has_expires,
                    record.is_persistent,
                    record.priority,
                    record.samesite,
                    record.source_scheme,
                    record.source_port,
                    record.last_update_utc,
                    record.source_type,
                    record.has_cross_site_ancestor,
                ],
            )
            .map_err(|error| format!("failed to restore Claude Desktop cookie: {error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("failed to save Claude Desktop cookies: {error}"))
}

fn clear_auth_cookies(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut connection = Connection::open(path)
        .map_err(|error| format!("failed to open Claude Desktop cookies: {error}"))?;
    let transaction = connection
        .transaction()
        .map_err(|error| format!("failed to update Claude Desktop cookies: {error}"))?;
    delete_auth_cookies(&transaction)?;
    transaction
        .commit()
        .map_err(|error| format!("failed to save Claude Desktop cookies: {error}"))
}

fn delete_auth_cookies(connection: &Connection) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM cookies \
             WHERE (host_key = 'claude.ai' OR host_key = '.claude.ai') \
               AND name IN (?1, ?2, ?3, ?4, ?5)",
            params![
                AUTH_COOKIE_NAMES[0],
                AUTH_COOKIE_NAMES[1],
                AUTH_COOKIE_NAMES[2],
                AUTH_COOKIE_NAMES[3],
                AUTH_COOKIE_NAMES[4],
            ],
        )
        .map(|_| ())
        .map_err(|error| format!("failed to clear Claude Desktop authentication cookies: {error}"))
}

fn ensure_cookie_schema(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS meta( \
                key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, \
                value LONGVARCHAR \
             ); \
             INSERT OR IGNORE INTO meta(key, value) VALUES \
                ('version', '24'), \
                ('last_compatible_version', '24'); \
             CREATE TABLE IF NOT EXISTS cookies( \
                creation_utc INTEGER NOT NULL, \
                host_key TEXT NOT NULL, \
                top_frame_site_key TEXT NOT NULL, \
                name TEXT NOT NULL, \
                value TEXT NOT NULL, \
                encrypted_value BLOB NOT NULL, \
                path TEXT NOT NULL, \
                expires_utc INTEGER NOT NULL, \
                is_secure INTEGER NOT NULL, \
                is_httponly INTEGER NOT NULL, \
                last_access_utc INTEGER NOT NULL, \
                has_expires INTEGER NOT NULL, \
                is_persistent INTEGER NOT NULL, \
                priority INTEGER NOT NULL, \
                samesite INTEGER NOT NULL, \
                source_scheme INTEGER NOT NULL, \
                source_port INTEGER NOT NULL, \
                last_update_utc INTEGER NOT NULL, \
                source_type INTEGER NOT NULL, \
                has_cross_site_ancestor INTEGER NOT NULL \
             ); \
             CREATE UNIQUE INDEX IF NOT EXISTS cookies_unique_index \
             ON cookies( \
                host_key, top_frame_site_key, has_cross_site_ancestor, name, path, \
                source_scheme, source_port \
             );",
        )
        .map_err(|error| format!("Claude Desktop cookie schema is unsupported: {error}"))
}

fn cookie_database_version(connection: &Connection) -> Result<u32, String> {
    let version = connection
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| format!("Claude Desktop cookie schema is unsupported: {error}"))?;
    let version = version
        .parse()
        .map_err(|_| "Claude Desktop cookie database version is invalid".to_string())?;
    Ok(version)
}

fn cookie_database_version_for_capture(
    connection: &Connection,
) -> Result<(u32, Option<String>), String> {
    let version = cookie_database_version(connection)?;
    let warning = (version != SUPPORTED_COOKIE_DATABASE_VERSION).then(|| {
        format!(
            "Claude Desktop Cookie database version is {version}. YAAT is currently adapted to version {SUPPORTED_COOKIE_DATABASE_VERSION}; the credential was readable, but YAAT will not write this Cookie database until support is updated."
        )
    });
    Ok((version, warning))
}

fn cookie_database_version_for_restore(connection: &Connection) -> Result<u32, String> {
    let version = cookie_database_version(connection)?;
    if version != SUPPORTED_COOKIE_DATABASE_VERSION {
        return Err(format!(
            "Claude Desktop Cookie database is version {version}, but this YAAT version supports version {SUPPORTED_COOKIE_DATABASE_VERSION}; update YAAT before importing or switching this account"
        ));
    }
    Ok(version)
}

fn validate_cookie_database(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to open Claude Desktop cookies: {error}"))?;
    cookie_database_version_for_restore(&connection).map(|_| ())
}

fn cookie_plaintext(
    host_key: &str,
    database_version: u32,
    value: &str,
) -> Result<Zeroizing<Vec<u8>>, String> {
    let hash_bytes = if database_version >= 24 { 32 } else { 0 };
    let capacity = value
        .len()
        .checked_add(hash_bytes)
        .ok_or_else(|| "Claude Desktop authentication cookie is unexpectedly large".to_string())?;
    let mut plaintext = Zeroizing::new(Vec::with_capacity(capacity));
    if database_version >= 24 {
        plaintext.extend_from_slice(&Sha256::digest(host_key.as_bytes()));
    }
    plaintext.extend_from_slice(value.as_bytes());
    Ok(plaintext)
}

fn cookie_value<'a>(
    host_key: &str,
    database_version: u32,
    plaintext: &'a [u8],
) -> Result<&'a [u8], String> {
    if database_version < 24 {
        return Ok(plaintext);
    }
    if plaintext.len() < 32 {
        return Err("Claude Desktop authentication cookie is malformed".into());
    }
    let (hash, value) = plaintext.split_at(32);
    let expected = Sha256::digest(host_key.as_bytes());
    if hash != &expected[..] {
        return Err("Claude Desktop authentication cookie domain does not match".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str) -> CookieRecord {
        CookieRecord {
            creation_utc: 1,
            host_key: ".claude.ai".into(),
            top_frame_site_key: String::new(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            expires_utc: 2,
            is_secure: 1,
            is_httponly: 1,
            last_access_utc: 3,
            has_expires: 1,
            is_persistent: 1,
            priority: 1,
            samesite: 0,
            source_scheme: 2,
            source_port: 443,
            last_update_utc: 4,
            source_type: 0,
            has_cross_site_ancestor: 0,
        }
    }

    #[test]
    fn cookie_restore_replaces_only_authentication_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(COOKIES_FILE);
        restore_cookies_with_codec(&path, &[cookie("sessionKey", "account-a")], |value| {
            Ok(value.to_vec())
        })
        .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO cookies( \
                    creation_utc, host_key, top_frame_site_key, name, value, encrypted_value, \
                    path, expires_utc, is_secure, is_httponly, last_access_utc, has_expires, \
                    is_persistent, priority, samesite, source_scheme, source_port, \
                    last_update_utc, source_type, has_cross_site_ancestor \
                 ) VALUES (1, '.claude.ai', '', 'theme', '', x'01', '/', 2, 0, 0, 3, 1, 1, 1, 0, 2, 443, 4, 0, 0)",
                [],
            )
            .unwrap();
        drop(connection);

        restore_cookies_with_codec(&path, &[cookie("sessionKey", "account-b")], |value| {
            Ok(value.to_vec())
        })
        .unwrap();
        let connection = Connection::open(&path).unwrap();
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM cookies", [], |row| row.get(0))
            .unwrap();
        let auth: Vec<u8> = connection
            .query_row(
                "SELECT encrypted_value FROM cookies WHERE name = 'sessionKey'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
        let expected = cookie_plaintext(".claude.ai", 24, "account-b").unwrap();
        assert_eq!(auth, expected.as_slice());

        drop(connection);
        let (captured, warning) =
            capture_cookies_with_codec(&path, |value| Ok(value.to_vec())).unwrap();
        assert!(warning.is_none());
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].value, "account-b");
        let serialized = serde_json::to_value(&captured[0]).unwrap();
        assert!(serialized.get("encrypted_value").is_none());
    }

    #[test]
    fn readable_new_cookie_database_versions_return_a_warning() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(COOKIES_FILE);
        let connection = Connection::open(&path).unwrap();
        ensure_cookie_schema(&connection).unwrap();
        connection
            .execute("UPDATE meta SET value = '25' WHERE key = 'version'", [])
            .unwrap();
        drop(connection);

        let (records, warning) =
            capture_cookies_with_codec(&path, |value| Ok(value.to_vec())).unwrap();
        assert!(records.is_empty());
        let warning = warning.unwrap();
        assert!(warning.contains("version is 25"));
        assert!(warning.contains("adapted to version 24"));

        let error = validate_cookie_database(&path).unwrap_err();
        assert!(error.contains("version 25"));
        assert!(error.contains("supports version 24"));
    }

    #[test]
    fn legacy_encrypted_cookie_snapshots_are_not_silently_accepted() {
        let mut value = serde_json::to_value(cookie("sessionKey", "plaintext")).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("encrypted_value".into(), serde_json::json!([1, 2, 3]));
        assert!(serde_json::from_value::<CookieRecord>(value).is_err());
    }

    #[test]
    fn clear_removes_only_owned_config_keys() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(CONFIG_STORE_FILE),
            br#"{"theme":"keep","lastKnownAccountUuid":"00000000-0000-4000-8000-000000000001","oauth:tokenCache":"secret"}"#,
        )
        .unwrap();
        clear(temp.path()).unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(temp.path().join(CONFIG_STORE_FILE)).unwrap())
                .unwrap();
        assert_eq!(value["theme"], "keep");
        assert!(value.get(ACCOUNT_UUID_KEY).is_none());
        assert!(value.get(OAUTH_CACHE_KEY).is_none());
    }
}
