//! Ordered SQLite schema migrations.

use rusqlite::{Connection, TransactionBehavior, params};

use super::{DbError, now_millis};

pub(super) const LATEST_SCHEMA_VERSION: i64 = 1;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial_store",
    sql: r#"
        CREATE TABLE providers (
            id              TEXT PRIMARY KEY NOT NULL,
            platform        TEXT NOT NULL CHECK (platform IN ('codex', 'claude_code', 'claude_desktop')),
            kind            TEXT NOT NULL CHECK (kind IN ('official_subscription', 'official_api', 'third_party')),
            name            TEXT NOT NULL CHECK (length(trim(name)) > 0),
            account_label   TEXT,
            base_url        TEXT,
            model           TEXT,
            custom_headers  TEXT NOT NULL DEFAULT '[]',
            user_agent      TEXT,
            platform_config TEXT NOT NULL,
            secret_kind     TEXT NOT NULL CHECK (secret_kind IN ('none', 'api_key', 'bearer_token')),
            profile_home    TEXT,
            status          TEXT NOT NULL CHECK (status IN ('ready', 'needs_login')),
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL
        );

        CREATE INDEX providers_platform_idx
            ON providers(platform, updated_at DESC);

        CREATE TABLE platform_bindings (
            platform        TEXT PRIMARY KEY NOT NULL CHECK (platform IN ('codex', 'claude_code', 'claude_desktop')),
            global_profile_id TEXT,
            last_managed_profile_id TEXT,
            updated_at      INTEGER NOT NULL,
            FOREIGN KEY(global_profile_id) REFERENCES providers(id) ON DELETE RESTRICT,
            FOREIGN KEY(last_managed_profile_id) REFERENCES providers(id) ON DELETE SET NULL
        );

        INSERT INTO platform_bindings(platform, global_profile_id, last_managed_profile_id, updated_at)
        VALUES
            ('codex', NULL, NULL, 0),
            ('claude_code', NULL, NULL, 0),
            ('claude_desktop', NULL, NULL, 0);

        CREATE TABLE settings (
            singleton_id            INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
            language                TEXT NOT NULL,
            theme                   TEXT NOT NULL,
            timezone                TEXT NOT NULL,
            default_activation_mode TEXT NOT NULL CHECK (default_activation_mode IN ('managed_launch', 'global_credential')),
            codex_path              TEXT,
            claude_path             TEXT,
            claude_desktop_path     TEXT,
            codex_home              TEXT,
            claude_config_dir       TEXT,
            unify_codex_history      INTEGER NOT NULL DEFAULT 0 CHECK (unify_codex_history IN (0, 1)),
            unify_claude_code_history INTEGER NOT NULL DEFAULT 0 CHECK (unify_claude_code_history IN (0, 1)),
            unify_claude_desktop_code_history INTEGER NOT NULL DEFAULT 0 CHECK (unify_claude_desktop_code_history IN (0, 1)),
            claude_desktop_history_target TEXT,
            usage_refresh_interval_seconds INTEGER NOT NULL DEFAULT 0
                CHECK (usage_refresh_interval_seconds IN (0, 5, 10, 30, 60)),
            updated_at              INTEGER NOT NULL
        );

        CREATE TABLE account_data (
            record_id       TEXT PRIMARY KEY NOT NULL,
            profile_id      TEXT NOT NULL,
            provider_id     TEXT,
            record_kind     TEXT NOT NULL,
            value           BLOB NOT NULL,
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            FOREIGN KEY(provider_id) REFERENCES providers(id) ON DELETE CASCADE
        );

        CREATE INDEX account_data_provider_idx ON account_data(provider_id);

        CREATE TABLE usage_events (
            platform                 TEXT NOT NULL CHECK (platform IN ('codex', 'claude_code', 'claude_desktop')),
            event_id                 TEXT NOT NULL,
            model                    TEXT,
            occurred_at              INTEGER NOT NULL,
            uncached_input           INTEGER NOT NULL CHECK (uncached_input >= 0),
            cache_read               INTEGER NOT NULL CHECK (cache_read >= 0),
            cache_write              INTEGER NOT NULL CHECK (cache_write >= 0),
            output                   INTEGER NOT NULL CHECK (output >= 0),
            reasoning_output         INTEGER NOT NULL CHECK (reasoning_output >= 0),
            request_count            INTEGER NOT NULL CHECK (request_count >= 0),
            created_at               INTEGER NOT NULL,
            PRIMARY KEY(platform, event_id)
        );

        CREATE INDEX usage_events_platform_time_idx
            ON usage_events(platform, occurred_at);

        CREATE INDEX usage_events_platform_model_time_idx
            ON usage_events(platform, model, occurred_at);

        CREATE TABLE usage_sources (
            platform                 TEXT NOT NULL CHECK (platform IN ('codex', 'claude_code', 'claude_desktop')),
            source_path              TEXT NOT NULL,
            file_size                INTEGER NOT NULL CHECK (file_size >= 0),
            modified_at              INTEGER NOT NULL,
            fingerprint              TEXT NOT NULL,
            malformed_records        INTEGER NOT NULL DEFAULT 0 CHECK (malformed_records >= 0),
            scanned_at               INTEGER NOT NULL,
            PRIMARY KEY(platform, source_path)
        );

        CREATE TABLE usage_event_sources (
            platform                 TEXT NOT NULL,
            event_id                 TEXT NOT NULL,
            source_path              TEXT NOT NULL,
            PRIMARY KEY(platform, event_id, source_path),
            FOREIGN KEY(platform, event_id) REFERENCES usage_events(platform, event_id) ON DELETE CASCADE,
            FOREIGN KEY(platform, source_path) REFERENCES usage_sources(platform, source_path) ON DELETE CASCADE
        );

        CREATE INDEX usage_event_sources_path_idx
            ON usage_event_sources(platform, source_path);

        CREATE TABLE usage_scan_summary (
            platform                 TEXT NOT NULL CHECK (platform IN ('codex', 'claude_code', 'claude_desktop')),
            files_scanned            INTEGER NOT NULL DEFAULT 0 CHECK (files_scanned >= 0),
            malformed_records        INTEGER NOT NULL DEFAULT 0 CHECK (malformed_records >= 0),
            duplicate_records        INTEGER NOT NULL DEFAULT 0 CHECK (duplicate_records >= 0),
            coverage_start           INTEGER,
            coverage_end             INTEGER,
            last_scanned_at           INTEGER,
            is_partial               INTEGER NOT NULL DEFAULT 0 CHECK (is_partial IN (0, 1)),
            PRIMARY KEY(platform)
        );

        CREATE TABLE history_sources (
            scope                     TEXT NOT NULL CHECK (scope IN ('codex', 'claude_code', 'claude_desktop_code')),
            root_id                   TEXT NOT NULL,
            source_path               TEXT NOT NULL,
            session_key               TEXT NOT NULL,
            file_size                 INTEGER NOT NULL CHECK (file_size >= 0),
            modified_at               INTEGER NOT NULL,
            fingerprint               TEXT NOT NULL,
            processed_at              INTEGER NOT NULL,
            PRIMARY KEY(scope, root_id, source_path)
        );

        CREATE TABLE history_sync_status (
            scope                     TEXT PRIMARY KEY NOT NULL CHECK (scope IN ('codex', 'claude_code', 'claude_desktop_code')),
            state                     TEXT NOT NULL,
            processed_files           INTEGER NOT NULL DEFAULT 0,
            last_completed_at          INTEGER,
            error_summary             TEXT
        );
    "#,
}];

pub(super) fn apply(connection: &mut Connection) -> Result<(), DbError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations ( \
             version INTEGER PRIMARY KEY NOT NULL, \
             name TEXT NOT NULL, \
             applied_at INTEGER NOT NULL \
         );",
    )?;

    let current: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(DbError::DatabaseTooNew {
            found: current,
            supported: LATEST_SCHEMA_VERSION,
        });
    }

    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
    {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![migration.version, migration.name, now_millis()],
        )?;
        transaction.commit()?;
    }

    Ok(())
}
