//! Validation shared by Tauri commands and platform adapters.

use url::Url;
use yaat_contracts::{
    CreateProviderRequest, ProviderKind, ProviderProfile, SecretKind, UpdateProviderRequest,
};

use crate::error::{AppError, AppResult};

pub fn validate_create(request: &CreateProviderRequest) -> AppResult<()> {
    validate_name(&request.name)?;
    validate_account_label(request.account_label.as_deref())?;
    validate_official_credential_field(request.kind, request.official_credential.as_deref())?;
    validate_profile_shape(
        request.kind,
        request.base_url.as_deref(),
        request.model.as_deref(),
        request.secret_kind,
        request.secret.as_deref(),
        false,
    )
}

pub fn validate_update(request: &UpdateProviderRequest) -> AppResult<()> {
    crate::paths::validate_identifier(&request.id)?;
    validate_name(&request.name)?;
    validate_account_label(request.account_label.as_deref())?;
    if let Some(secret) = request.replacement_secret.as_deref() {
        validate_secret(secret)?;
    }
    if let Some(base_url) = request.base_url.as_deref() {
        validate_provider_url(base_url)?;
    }
    validate_optional_model(request.model.as_deref())
}

pub fn validate_existing_profile_update(
    current: &ProviderProfile,
    request: &UpdateProviderRequest,
) -> AppResult<()> {
    validate_official_credential_field(
        current.kind,
        request.replacement_official_credential.as_deref(),
    )?;
    validate_profile_shape(
        current.kind,
        request.base_url.as_deref(),
        request.model.as_deref(),
        request.secret_kind,
        request.replacement_secret.as_deref(),
        current.has_secret,
    )
}

fn validate_official_credential_field(
    kind: ProviderKind,
    credential: Option<&str>,
) -> AppResult<()> {
    if credential.is_some() && kind != ProviderKind::OfficialSubscription {
        return Err(AppError::Validation(
            "official credential JSON can only be used by subscription accounts".into(),
        ));
    }
    Ok(())
}

pub fn validate_name(name: &str) -> AppResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 80 || trimmed.chars().any(char::is_control) {
        return Err(AppError::Validation(
            "profile name must contain 1-80 printable characters".into(),
        ));
    }
    Ok(())
}

pub fn validate_account_label(label: Option<&str>) -> AppResult<()> {
    if let Some(label) = label
        && (label.chars().count() > 160 || label.chars().any(char::is_control))
    {
        return Err(AppError::Validation(
            "account label must be at most 160 printable characters".into(),
        ));
    }
    Ok(())
}

fn validate_profile_shape(
    kind: ProviderKind,
    base_url: Option<&str>,
    model: Option<&str>,
    secret_kind: SecretKind,
    secret: Option<&str>,
    secret_already_exists: bool,
) -> AppResult<()> {
    match kind {
        ProviderKind::OfficialSubscription => {
            if base_url.is_some() || secret.is_some() || secret_kind != SecretKind::None {
                return Err(AppError::Validation(
                    "subscription profiles do not use API/provider credentials".into(),
                ));
            }
        }
        ProviderKind::OfficialApi => {
            if base_url.is_some() {
                return Err(AppError::Validation(
                    "official API profiles use the platform's official endpoint".into(),
                ));
            }
            if secret_kind == SecretKind::None || (!secret_already_exists && secret.is_none()) {
                return Err(AppError::Validation(
                    "an official API profile requires a credential".into(),
                ));
            }
        }
        ProviderKind::ThirdParty => {
            let base_url = base_url.ok_or_else(|| {
                AppError::Validation("a third-party profile requires a base URL".into())
            })?;
            validate_provider_url(base_url)?;
            validate_optional_model(model)?;
            if model.is_none() {
                return Err(AppError::Validation(
                    "a third-party profile requires a model identifier".into(),
                ));
            }
            if secret_kind == SecretKind::None || (!secret_already_exists && secret.is_none()) {
                return Err(AppError::Validation(
                    "a third-party profile requires its own credential".into(),
                ));
            }
        }
    }
    if let Some(secret) = secret {
        validate_secret(secret)?;
    }
    validate_optional_model(model)
}

pub fn validate_provider_url(value: &str) -> AppResult<Url> {
    if value.chars().count() > 2048 || value.chars().any(char::is_control) {
        return Err(AppError::Validation("provider URL is invalid".into()));
    }
    let url =
        Url::parse(value).map_err(|_| AppError::Validation("provider URL is invalid".into()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::Validation(
            "provider URL must not contain credentials".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AppError::Validation(
            "provider URL must not contain a query or fragment".into(),
        ));
    }
    match url.scheme() {
        "https" | "http" if url.host().is_some() => {}
        _ => {
            return Err(AppError::Validation(
                "provider URL must be an absolute HTTP or HTTPS URL".into(),
            ));
        }
    }
    Ok(url)
}

fn validate_optional_model(model: Option<&str>) -> AppResult<()> {
    if let Some(model) = model
        && (model.trim().is_empty()
            || model.chars().count() > 256
            || model.chars().any(char::is_control))
    {
        return Err(AppError::Validation(
            "model identifier must contain 1-256 printable characters".into(),
        ));
    }
    Ok(())
}

fn validate_secret(secret: &str) -> AppResult<()> {
    if secret.is_empty() || secret.len() > 16 * 1024 || secret.contains(['\r', '\n', '\0']) {
        return Err(AppError::Validation("credential value is invalid".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rejects_credentials_and_accepts_http_gateways() {
        assert!(validate_provider_url("https://token@example.com/v1").is_err());
        assert!(validate_provider_url("http://example.com/v1").is_ok());
        assert!(validate_provider_url("http://127.0.0.1:4000/v1").is_ok());
        assert!(validate_provider_url("https://api.example.com/v1").is_ok());
    }
}
