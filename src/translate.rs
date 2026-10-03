use serde_json::{json, Value};
use uuid::Uuid;

/// Convert an OpenAI-format chat completions request into Antigravity's
/// Gemini-style request body.
pub fn openai_to_antigravity(
    openai_body: &Value,
    antigravity_model: &str,
    project_id: &str,
) -> Value {
    let messages = openai_body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    // Separate system instruction from conversation
    let mut system_text = String::new();
    let mut contents: Vec<Value> = Vec::new();

    for msg in &messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = extract_content_text(msg);
    let content = crate::rtk::compress_text(&content);

        match role {
            "system" => {
                if !system_text.is_empty() {
                    system_text.push('\n');
                }
                system_text.push_str(&content);
            }
            "user" => {
                contents.push(json!({
                    "role": "user",
                    "parts": [{"text": content}]
                }));
            }
            "assistant" => {
                contents.push(json!({
                    "role": "model",
                    "parts": [{"text": content}]
                }));
            }
            _ => {
                // tool, function, etc. — treat as user for now
                contents.push(json!({
                    "role": "user",
                    "parts": [{"text": content}]
                }));
            }
        }
    }

    // Build generationConfig
    let mut gen_config = json!({});
    if let Some(temp) = openai_body.get("temperature") {
        gen_config["temperature"] = temp.clone();
    }
    if let Some(max_tokens) = openai_body.get("max_tokens") {
        gen_config["maxOutputTokens"] = max_tokens.clone();
    }
    if let Some(top_p) = openai_body.get("top_p") {
        gen_config["topP"] = top_p.clone();
    }
    if let Some(stop) = openai_body.get("stop") {
        gen_config["stopSequences"] = stop.clone();
    }

    let mut request = json!({
        "contents": contents,
        "generationConfig": gen_config,
    });

    if !system_text.is_empty() {
        request["systemInstruction"] = json!({
            "parts": [{"text": system_text}]
        });
    }

    let request_id = Uuid::new_v4().to_string();

    let mut body = json!({
        "model": antigravity_model,
        "request": request,
        "userAgent": "antigravity",
        "requestId": request_id,
    });

    if !project_id.is_empty() {
        body["project"] = json!(project_id);
    }

    body
}

/// Extract text content from an OpenAI message (handles both string and array content)
fn extract_content_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Convert Antigravity's non-streaming response to OpenAI format.
pub fn antigravity_to_openai(
    antigravity_resp: &Value,
    model: &str,
) -> Value {
    // Code Assist / Antigravity wraps the payload inside a top-level `response` field
    let inner = antigravity_resp
        .get("response")
        .unwrap_or(antigravity_resp);

    let candidate = inner
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first());

    let content = candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            let mut text = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            text
        })
        .unwrap_or_default();

    let finish_reason = candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(|f| f.as_str())
        .map(map_finish_reason)
        .unwrap_or("stop");

    let usage = inner.get("usageMetadata").or_else(|| antigravity_resp.get("usageMetadata"));
    let prompt_tokens = usage
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|u| u.get("totalTokenCount"))
        .and_then(|t| t.as_i64())
        .unwrap_or(prompt_tokens + completion_tokens);

    let id = format!("chatcmpl-{}", Uuid::new_v4());
    let created = chrono::Utc::now().timestamp();

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
            },
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
        }
    })
}

/// Convert a streaming SSE chunk from Antigravity to OpenAI delta format.
pub fn antigravity_chunk_to_openai_delta(
    chunk: &Value,
    model: &str,
    chunk_id: &str,
) -> Option<Value> {
    let inner = chunk.get("response").unwrap_or(chunk);

    let candidate = inner
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())?;

    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array());

    let mut text = String::new();
    if let Some(parts) = parts {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
        }
    }

    let finish_reason = candidate
        .get("finishReason")
        .and_then(|f| f.as_str())
        .and_then(|r| {
            if r == "STOP" || r == "MAX_TOKENS" || r == "SAFETY" {
                Some(map_finish_reason(r))
            } else {
                None
            }
        });

    let mut delta = json!({});
    if !text.is_empty() {
        delta["content"] = json!(text);
        delta["role"] = json!("assistant");
    }

    let created = chrono::Utc::now().timestamp();

    Some(json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    }))
}

fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" => "content_filter",
        "RECITATION" => "content_filter",
        _ => "stop",
    }
}
