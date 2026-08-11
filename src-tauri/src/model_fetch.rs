//! Sanitized model discovery for third-party providers.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use url::Url;
use yaat_contracts::{FetchedModel, ModelFetchRequest, ModelFetchResponse, Platform, SecretKind};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 512;

#[derive(Deserialize)]
struct ModelEnvelope {
    #[serde(default)]
    data: Vec<ModelItem>,
    #[serde(default)]
    models: Vec<ModelItem>,
}

#[derive(Deserialize)]
struct ModelItem {
    id: String,
    #[serde(default)]
    owned_by: Option<String>,
}

pub async fn fetch(request: &ModelFetchRequest) -> Result<ModelFetchResponse, String> {
    crate::validation::validate_model_fetch(request).map_err(|error| error.to_string())?;
    let base = validate_base_url(&request.base_url)?;
    let headers = request_headers(request)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("unable to initialize model discovery: {error}"))?;

    let primary = model_url(&base, true)?;
    let mut response = client
        .get(primary)
        .headers(headers.clone())
        .send()
        .await
        .map_err(sanitize_transport_error)?;
    if matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        response = client
            .get(model_url(&base, false)?)
            .headers(headers)
            .send()
            .await
            .map_err(sanitize_transport_error)?;
    }
    let status = response.status();
    let bytes = read_limited(response).await?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes);
        let detail = sanitize_error_detail(&detail, request);
        return Err(format!(
            "model discovery returned HTTP {status}: {}",
            truncate(&detail, MAX_ERROR_BYTES)
        ));
    }
    let envelope: ModelEnvelope = serde_json::from_slice(&bytes)
        .map_err(|_| "model discovery returned an unsupported JSON response".to_string())?;
    let mut models = BTreeMap::<String, FetchedModel>::new();
    for item in envelope.data.into_iter().chain(envelope.models) {
        let id = item.id.trim();
        if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
            continue;
        }
        let direct_compatible =
            request.platform != Platform::ClaudeDesktop || is_desktop_direct_model(id);
        models.entry(id.to_owned()).or_insert_with(|| FetchedModel {
            id: id.to_owned(),
            owned_by: item.owned_by,
            direct_compatible,
            warning: (!direct_compatible).then(|| "requires routing".into()),
        });
    }
    Ok(ModelFetchResponse {
        models: models.into_values().collect(),
    })
}

fn validate_base_url(value: &str) -> Result<Url, String> {
    let mut url = Url::parse(value.trim()).map_err(|_| "Base URL is invalid".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Base URL must be an absolute HTTP(S) URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Base URL must not contain credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Base URL must not contain a query or fragment".into());
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn model_url(base: &Url, primary: bool) -> Result<Url, String> {
    let mut url = base.clone();
    let mut segments = url
        .path_segments()
        .ok_or_else(|| "Base URL cannot be used for model discovery".to_string())?
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let ends_version = segments.last().is_some_and(|segment| {
        segment.strip_prefix('v').is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    });
    if primary && !ends_version {
        segments.push("v1".into());
    }
    segments.push("models".into());
    url.set_path(&segments.join("/"));
    Ok(url)
}

fn request_headers(request: &ModelFetchRequest) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for entry in &request.custom_headers {
        let name = HeaderName::from_bytes(entry.name.as_bytes())
            .map_err(|_| format!("invalid custom header name: {}", entry.name))?;
        let value = HeaderValue::from_str(&entry.value)
            .map_err(|_| format!("invalid value for custom header: {}", entry.name))?;
        if name == USER_AGENT
            || name == AUTHORIZATION
            || name.as_str().eq_ignore_ascii_case("x-api-key")
        {
            return Err(format!(
                "custom header conflicts with a managed field: {}",
                entry.name
            ));
        }
        headers.insert(name, value);
    }
    if let Some(user_agent) = request
        .user_agent
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(user_agent.trim())
                .map_err(|_| "User-Agent is invalid".to_string())?,
        );
    }
    match (request.platform, request.secret_kind) {
        (_, SecretKind::BearerToken) | (Platform::Codex, SecretKind::ApiKey) => {
            let value = HeaderValue::from_str(&format!("Bearer {}", request.credential))
                .map_err(|_| "provider credential is invalid".to_string())?;
            headers.insert(AUTHORIZATION, value);
        }
        (Platform::ClaudeCode | Platform::ClaudeDesktop, SecretKind::ApiKey) => {
            headers.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(&request.credential)
                    .map_err(|_| "provider credential is invalid".to_string())?,
            );
        }
        (_, SecretKind::None) => return Err("model discovery requires a credential".into()),
    }
    Ok(headers)
}

async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("model discovery response exceeds 1 MiB".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "unable to read model discovery response".to_string())?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("model discovery response exceeds 1 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn sanitize_transport_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "model discovery timed out".into()
    } else {
        "model discovery request failed".into()
    }
}

fn truncate(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect::<String>()
}

fn sanitize_error_detail(value: &str, request: &ModelFetchRequest) -> String {
    let mut sanitized = value.replace(&request.credential, "<redacted>");
    for header in &request.custom_headers {
        if !header.value.is_empty() {
            sanitized = sanitized.replace(&header.value, "<redacted>");
        }
    }
    sanitized
}

fn is_desktop_direct_model(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    let Some(tail) = normalized
        .strip_prefix("anthropic/claude-")
        .or_else(|| normalized.strip_prefix("claude-"))
    else {
        return false;
    };
    ["sonnet-", "opus-", "haiku-", "fable-"]
        .iter()
        .any(|prefix| {
            tail.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use yaat_contracts::HeaderEntry;

    use super::*;

    #[derive(Clone)]
    struct MockResponse {
        status: &'static str,
        body: String,
    }

    fn request(platform: Platform, base_url: String, secret_kind: SecretKind) -> ModelFetchRequest {
        ModelFetchRequest {
            platform,
            base_url,
            secret_kind,
            credential: "discovery-secret".into(),
            custom_headers: vec![HeaderEntry {
                name: "X-Tenant".into(),
                value: "tenant-a".into(),
            }],
            user_agent: Some("YAAT-Test/1".into()),
        }
    }

    fn mock_server(responses: Vec<MockResponse>) -> (String, mpsc::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 2048];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut chunk).unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let reply = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
            sender.send(requests).unwrap();
        });
        (format!("http://{address}"), receiver)
    }

    #[test]
    fn fetches_openai_models_with_direct_headers_and_stable_deduplication() {
        let (base_url, received) = mock_server(vec![MockResponse {
            status: "200 OK",
            body: serde_json::json!({
                "data": [
                    {"id": "z-model", "owned_by": "vendor"},
                    {"id": "a-model"},
                    {"id": "a-model", "owned_by": "duplicate"}
                ]
            })
            .to_string(),
        }]);
        let response = tauri::async_runtime::block_on(fetch(&request(
            Platform::Codex,
            base_url,
            SecretKind::ApiKey,
        )))
        .unwrap();
        assert_eq!(
            response
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-model", "z-model"]
        );
        let requests = received.recv().unwrap();
        assert!(requests[0].starts_with("GET /v1/models HTTP/1.1"));
        let lower = requests[0].to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer discovery-secret"));
        assert!(lower.contains("x-tenant: tenant-a"));
        assert!(lower.contains("user-agent: yaat-test/1"));
    }

    #[test]
    fn falls_back_to_base_models_and_marks_desktop_routing_requirements() {
        let (base_url, received) = mock_server(vec![
            MockResponse {
                status: "404 Not Found",
                body: "{}".into(),
            },
            MockResponse {
                status: "200 OK",
                body: serde_json::json!({
                    "models": [
                        {"id": "claude-sonnet-4-5"},
                        {"id": "upstream/private-model"}
                    ]
                })
                .to_string(),
            },
        ]);
        let response = tauri::async_runtime::block_on(fetch(&request(
            Platform::ClaudeDesktop,
            base_url,
            SecretKind::ApiKey,
        )))
        .unwrap();
        assert!(response.models[0].direct_compatible);
        assert!(!response.models[1].direct_compatible);
        assert_eq!(
            response.models[1].warning.as_deref(),
            Some("requires routing")
        );
        let requests = received.recv().unwrap();
        assert!(requests[0].starts_with("GET /v1/models HTTP/1.1"));
        assert!(requests[1].starts_with("GET /models HTTP/1.1"));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-api-key: discovery-secret")
        );
    }

    #[test]
    fn rejects_oversized_invalid_and_secret_echoing_responses() {
        let (base_url, _) = mock_server(vec![MockResponse {
            status: "200 OK",
            body: "x".repeat(MAX_RESPONSE_BYTES + 1),
        }]);
        let error = tauri::async_runtime::block_on(fetch(&request(
            Platform::Codex,
            base_url,
            SecretKind::BearerToken,
        )))
        .unwrap_err();
        assert!(error.contains("exceeds 1 MiB"));

        let (base_url, _) = mock_server(vec![MockResponse {
            status: "400 Bad Request",
            body: "credential discovery-secret and tenant tenant-a are invalid".into(),
        }]);
        let error = tauri::async_runtime::block_on(fetch(&request(
            Platform::Codex,
            base_url,
            SecretKind::BearerToken,
        )))
        .unwrap_err();
        assert!(!error.contains("discovery-secret"));
        assert!(!error.contains("tenant-a"));
        assert!(error.contains("<redacted>"));

        let (base_url, _) = mock_server(vec![MockResponse {
            status: "200 OK",
            body: "not-json".into(),
        }]);
        let error = tauri::async_runtime::block_on(fetch(&request(
            Platform::Codex,
            base_url,
            SecretKind::BearerToken,
        )))
        .unwrap_err();
        assert!(error.contains("unsupported JSON"));
    }

    #[test]
    fn validates_duplicate_reserved_and_hop_by_hop_headers() {
        let mut request = request(
            Platform::Codex,
            "https://example.test".into(),
            SecretKind::ApiKey,
        );
        request.custom_headers.push(HeaderEntry {
            name: "x-tenant".into(),
            value: "duplicate".into(),
        });
        assert!(crate::validation::validate_model_fetch(&request).is_err());
        request.custom_headers = vec![HeaderEntry {
            name: "Connection".into(),
            value: "close".into(),
        }];
        assert!(crate::validation::validate_model_fetch(&request).is_err());
    }
}
