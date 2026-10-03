use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::state::{LogKind, SharedState};
use crate::translate;

// Upstream API endpoints
// Note: daily-cloudcode-pa is Google's active dev endpoint that bypasses artificial prod 429 locks
// when daily quota fraction still remains (same technique used in 9router).
const CLOUDCODE_API: &str = "https://daily-cloudcode-pa.googleapis.com";
const USER_AGENT: &str = "antigravity/1.15.8 linux/amd64";
const API_CLIENT: &str = "google-cloud-sdk vscode_cloudshelleditor/0.1";
const CLIENT_METADATA: &str =
    r#"{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#;

/// POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Check master key auth if configured
    if let Some(ref master_key) = state.config.server.master_key {
        if !check_auth(&headers, master_key) {
            state.log(LogKind::Warn, "Incoming request rejected: invalid master key");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "Invalid or missing API key", "type": "auth_error"}})),
            )
                .into_response();
        }
    }

    // Extract model from request
    let request_model = match body.get("model").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => {
            state.log(LogKind::Warn, "Incoming request missing 'model' field");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": "Missing 'model' field", "type": "invalid_request_error"}})),
            )
                .into_response();
        }
    };

    let is_stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    state.log(
        LogKind::Incoming,
        format!(
            "POST /v1/chat/completions model={} stream={}",
            request_model, is_stream
        ),
    );

    // Resolve model alias
    let antigravity_model = state.config.resolve_model(&request_model);

    let account_count = state.config.antigravity_accounts().len();
    if account_count == 0 {
        state.log(LogKind::Error, "No accounts configured in pool");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "No accounts configured", "type": "proxy_error"}})),
        )
            .into_response();
    }
    let start_index = state.next_account_index();

    // Try each account with round-robin + failover
    for attempt in 0..account_count {
        let account_index = (start_index + attempt) % account_count;

        let (project_id, account_name) = {
            let accounts = state.oauth.accounts.read().await;
            let acc = &accounts[account_index];
            (acc.project_id.clone(), acc.name.clone())
        };

        // Get a fresh token
        let token = match state.oauth.get_token(account_index).await {
            Ok(t) => t,
            Err(e) => {
                state.log(
                    LogKind::Warn,
                    format!("Account [{}] token refresh failed: {}", account_name, e),
                );
                warn!(
                    account = %account_name,
                    error = %e,
                    "Token refresh failed, trying next account"
                );
                continue;
            }
        };

        // Translate request
        let antigravity_body =
            translate::openai_to_antigravity(&body, &antigravity_model, &project_id);

        // Build upstream URL
        let url = if is_stream {
            format!("{}/v1internal:streamGenerateContent?alt=sse", CLOUDCODE_API)
        } else {
            format!("{}/v1internal:generateContent", CLOUDCODE_API)
        };

        state.log(
            LogKind::Outgoing,
            format!(
                "Forwarding to Google CloudCode account=[{}] upstream_model={}",
                account_name, antigravity_model
            ),
        );

        info!(
            account = %account_name,
            model = %antigravity_model,
            stream = is_stream,
            "Proxying request"
        );

        // Build request
        let mut req = state
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .header("X-Goog-Api-Client", API_CLIENT)
            .header("Client-Metadata", CLIENT_METADATA);

        if is_stream {
            req = req.header("Accept", "text/event-stream");
        }

        let resp = match req.json(&antigravity_body).send().await {
            Ok(r) => r,
            Err(e) => {
                state.log(
                    LogKind::Error,
                    format!("Request to account [{}] network failed: {}", account_name, e),
                );
                error!(account = %account_name, error = %e, "Upstream request failed");
                continue;
            }
        };

        let status = resp.status();

        // Retry on 429, 5xx, or 401
        if status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
            || status == StatusCode::UNAUTHORIZED
        {
            let body_text = resp.text().await.unwrap_or_default();
            state.log(
                LogKind::Warn,
                format!(
                    "Upstream returned {} for account [{}] (retrying next account)",
                    status, account_name
                ),
            );
            warn!(
                account = %account_name,
                status = %status,
                body = %body_text,
                "Upstream error, trying next account"
            );

            // On 401, force token refresh for this account
            if status == StatusCode::UNAUTHORIZED {
                let _ = state.oauth.get_token(account_index).await;
            }
            continue;
        }

        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            state.log(
                LogKind::Error,
                format!("Upstream error {}: {}", status, body_text),
            );
            error!(
                account = %account_name,
                status = %status,
                body = %body_text,
                "Upstream returned error"
            );
            return (
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(json!({"error": {"message": body_text, "type": "upstream_error"}})),
            )
                .into_response();
        }

        state.log(
            LogKind::Success,
            format!(
                "Completed request via account=[{}] model={}",
                account_name, request_model
            ),
        );

        // Handle streaming response
        if is_stream {
            return stream_response(resp, &request_model).await;
        }

        // Handle non-streaming response
        let upstream_body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                state.log(LogKind::Error, "Failed to parse upstream JSON response");
                error!(error = %e, "Failed to parse upstream response");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": {"message": "Failed to parse upstream response", "type": "proxy_error"}})),
                )
                    .into_response();
            }
        };

        let openai_resp = translate::antigravity_to_openai(&upstream_body, &request_model);
        return Json(openai_resp).into_response();
    }

    state.log(
        LogKind::Error,
        "All accounts exhausted (all returned 429/quota or 5xx)",
    );

    // All accounts exhausted
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {"message": "All accounts exhausted (rate limited or auth failed)", "type": "proxy_error"}})),
    )
        .into_response()
}

/// Stream the Antigravity SSE response back as OpenAI-format SSE.
async fn stream_response(resp: reqwest::Response, model: &str) -> Response {
    let model = model.to_string();
    let chunk_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());

    let stream = resp.bytes_stream();

    // Buffer for incomplete SSE lines
    let model_clone = model.clone();
    let chunk_id_clone = chunk_id.clone();

    let transformed = async_stream::stream! {
        let mut buffer = String::new();
        let mut stream = stream;

        while let Some(chunk_result) = stream.next().await {
            let chunk: Bytes = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    error!(error = %e, "Stream read error");
                    break;
                }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(t) => t,
                Err(_) => continue,
            };

            buffer.push_str(text);

            // Process complete SSE lines
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim_end_matches('\r').to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        yield Ok::<Bytes, std::io::Error>(
                            Bytes::from("data: [DONE]\n\n")
                        );
                        continue;
                    }

                    if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                        if let Some(openai_chunk) = translate::antigravity_chunk_to_openai_delta(
                            &parsed,
                            &model_clone,
                            &chunk_id_clone,
                        ) {
                            let sse_line = format!("data: {}\n\n", serde_json::to_string(&openai_chunk).unwrap());
                            yield Ok::<Bytes, std::io::Error>(
                                Bytes::from(sse_line)
                            );
                        }
                    }
                }
            }
        }

        // Final DONE if not already sent
        yield Ok::<Bytes, std::io::Error>(Bytes::from("data: [DONE]\n\n"));
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(Body::from_stream(transformed))
        .unwrap()
}

/// GET /v1/models
pub async fn list_models(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    if let Some(ref master_key) = state.config.server.master_key {
        if !check_auth(&headers, master_key) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "Invalid or missing API key", "type": "auth_error"}})),
            )
                .into_response();
        }
    }

    let mut model_ids: Vec<String> = {
        let quotas = state.quotas.read().await;
        quotas
            .keys()
            .filter(|k| !k.starts_with("tab_") && !k.starts_with("chat_"))
            .cloned()
            .collect()
    };

    if model_ids.is_empty() {
        model_ids = vec![
            "gemini-3.8-flash-tiered".to_string(),
            "gemini-3.7-flash-tiered".to_string(),
            "gemini-3.6-flash-high".to_string(),
            "gemini-3.1-pro-high".to_string(),
            "claude-sonnet-4-6".to_string(),
            "claude-opus-4-6-thinking".to_string(),
            "gpt-oss-120b-medium".to_string(),
        ];
    }

    let models: Vec<Value> = model_ids
        .into_iter()
        .map(|name| {
            json!({
                "id": name,
                "object": "model",
                "created": 0,
                "owned_by": "antigravity",
            })
        })
        .collect();

    Json(json!({
        "object": "list",
        "data": models,
    }))
    .into_response()
}

/// GET /health
pub async fn health() -> &'static str {
    "ok"
}

fn check_auth(headers: &HeaderMap, master_key: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.strip_prefix("Bearer ")
                .map(|token| token == master_key)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}
