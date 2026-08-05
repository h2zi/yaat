//! Backend error taxonomy and its stable IPC representation.

use std::fmt::Display;

use thiserror::Error;
use yaat_contracts::ApiError;

use crate::db::DbError;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Validation(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("credential data is unavailable: {0}")]
    Credential(String),
    #[error("configuration is malformed: {0}")]
    ConfigMalformed(String),
    #[error("configuration operation blocked: {0}")]
    ConfigConflict(String),
    #[error("this client configuration version is not supported: {0}")]
    UnsupportedConfigVersion(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("usage data is unavailable: {0}")]
    UsageUnavailable(String),
    #[error("update check failed: {0}")]
    UpdateUnavailable(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("database error: {0}")]
    Database(String),
    #[error("command failed: {0}")]
    Command(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn io(error: impl Display) -> Self {
        Self::Io(error.to_string())
    }

    pub fn database(error: impl Display) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<AppError> for ApiError {
    fn from(error: AppError) -> Self {
        let code = match &error {
            AppError::Validation(_) => "validation",
            AppError::NotFound(_) => "not_found",
            AppError::Credential(_) => "credential_unavailable",
            AppError::ConfigMalformed(_) => "config_malformed",
            AppError::ConfigConflict(_) => "config_conflict",
            AppError::UnsupportedConfigVersion(_) => "unsupported_config_version",
            AppError::Io(_) => "io",
            AppError::UsageUnavailable(_) => "usage_source_unavailable",
            AppError::UpdateUnavailable(_) => "update_unavailable",
            AppError::Cancelled => "cancelled",
            AppError::Database(_) => "database",
            AppError::Command(_) => "command_failed",
            AppError::Internal(_) => "internal",
        };
        ApiError {
            code: code.into(),
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value.to_string())
    }
}

impl From<DbError> for AppError {
    fn from(value: DbError) -> Self {
        match value {
            DbError::NotFound { entity, id } => Self::NotFound(format!("{entity} '{id}'")),
            DbError::ProviderActive { id, platform } => {
                Self::ConfigConflict(format!("provider '{id}' is active for {platform}"))
            }
            DbError::PlatformMismatch {
                profile_id,
                expected,
                actual,
            } => Self::Validation(format!(
                "provider '{profile_id}' belongs to {actual}, not {expected}"
            )),
            DbError::InvalidInput(message) => Self::Validation(message.into()),
            DbError::DatabaseTooNew { found, supported } => Self::UnsupportedConfigVersion(
                format!("database schema {found} is newer than supported schema {supported}"),
            ),
            other => Self::Database(other.to_string()),
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;
