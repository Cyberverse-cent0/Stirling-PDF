//! Per-request user identity and LLM analytics. Port of
//! `stirling.services.tracking`: the contextvar half (`current_user_id` +
//! `require_current_user_id`) plus the PostHog `$ai_generation`/`$ai_trace`
//! capture that Python drives from an OpenTelemetry span processor. Rust has
//! no OTel spans, so `LlmClient` reports each provider call directly; the
//! trace id is scoped per HTTP request, matching how OTel context propagates
//! through nested agent runs.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::AppSettings;
use crate::llm::model::ProviderKind;

tokio::task_local! {
    static CURRENT_USER_ID: Option<String>;
    static CURRENT_TRACE_ID: String;
}

/// Run `fut` with `user_id` bound as the request principal.
pub async fn scoped<F: Future>(user_id: Option<String>, fut: F) -> F::Output {
    CURRENT_USER_ID.scope(user_id, fut).await
}

/// The request's user id, or `None` outside a scoped request / when the
/// `X-User-Id` header was absent.
pub fn current_user_id() -> Option<String> {
    CURRENT_USER_ID.try_with(|id| id.clone()).ok().flatten()
}

/// Fail-closed accessor for per-user storage boundaries: errors when no user
/// id is bound, mirroring Python's `RuntimeError`.
pub fn require_current_user_id() -> Result<String, String> {
    current_user_id().ok_or_else(|| {
        "X-User-Id is required for this operation but was not set on the request".to_string()
    })
}

/// Bind a fresh trace id for one HTTP request. Called once per request by
/// the user-id middleware; every LLM call underneath shares it, the same
/// grouping OTel context gives nested agent runs.
pub async fn scoped_trace<F: Future>(trace_id: String, fut: F) -> F::Output {
    CURRENT_TRACE_ID.scope(trace_id, fut).await
}

fn current_trace_id() -> Option<String> {
    CURRENT_TRACE_ID.try_with(|id| id.clone()).ok()
}

// ---- PostHog capture ----

struct PostHog {
    api_key: String,
    host: String,
    http: reqwest::Client,
    /// Trace ids already emitted as `$ai_trace` — Python's `_seen_traces`
    /// LRUSet, bounded the same way.
    seen_traces: Mutex<HashSet<String>>,
}

const SEEN_TRACES_CAP: usize = 10_000;

static POSTHOG: OnceLock<Option<PostHog>> = OnceLock::new();

/// Install the PostHog sink from settings; `None`/disabled leaves capture a
/// no-op. Called once at boot — Python's `setup_posthog_tracking`.
pub fn init(settings: &AppSettings) {
    let _ = POSTHOG.set(if settings.posthog_enabled && !settings.posthog_api_key.is_empty() {
        Some(PostHog {
            api_key: settings.posthog_api_key.clone(),
            host: settings.posthog_host.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            seen_traces: Mutex::new(HashSet::new()),
        })
    } else {
        None
    });
}

fn posthog() -> Option<&'static PostHog> {
    POSTHOG.get().and_then(|p| p.as_ref())
}

fn capture(event: &str, properties: Value) {
    let Some(ph) = posthog() else { return };
    let body = json!({
        "api_key": ph.api_key,
        "event": event,
        "distinct_id": current_user_id().unwrap_or_default(),
        "properties": properties,
    });
    let url = format!("{}/capture/", ph.host);
    let http = ph.http.clone();
    let api_key = ph.api_key.clone();
    tokio::spawn(async move {
        // PostHog authenticates via the api_key in the body for /capture/.
        let _ = api_key;
        if let Err(e) = http.post(&url).json(&body).send().await {
            tracing::debug!("posthog capture failed: {e}");
        }
    });
}

fn extract_user_message(input_messages: Option<&Value>) -> String {
    let Some(Value::Array(messages)) = input_messages else {
        return String::new();
    };
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(parts) = msg.get("parts").and_then(Value::as_array) {
            for part in parts.iter().rev() {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    return part
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        // OpenAI-shaped messages carry `content` directly.
        if let Some(content) = msg.get("content").and_then(Value::as_str) {
            return content.to_string();
        }
    }
    String::new()
}

fn transform_output_choices(output: &Value) -> Value {
    // pydantic-ai output messages -> PostHog choices shape; pass through
    // anything already list-shaped.
    output.clone()
}

/// Report one completed provider call. `request_body`/`response` are the raw
/// chat payloads; `latency` is wall time of the HTTP exchange.
pub fn on_llm_response(
    kind: ProviderKind,
    model_name: &str,
    base_url: &str,
    request_body: &Value,
    response: &Value,
    latency: Duration,
) {
    let Some(ph) = posthog() else { return };
    let _ = ph;

    let provider = match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenAiCompat => "openai",
    };
    let usage = response.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens").or_else(|| u.get("input_tokens")))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens").or_else(|| u.get("output_tokens")))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let response_model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(model_name);

    let mut properties = json!({
        "$ai_provider": provider,
        "$ai_model": response_model,
        "$ai_input_tokens": input_tokens,
        "$ai_output_tokens": output_tokens,
        "$ai_latency": latency.as_secs_f64(),
    });
    if let Some(trace_id) = current_trace_id() {
        properties["$ai_trace_id"] = json!(trace_id);
    }
    let base = base_url.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        properties["$ai_base_url"] = json!(rest);
    } else if let Some(rest) = base.strip_prefix("http://") {
        properties["$ai_base_url"] = json!(rest);
    } else {
        properties["$ai_base_url"] = json!(base);
    }

    let input_messages = request_body.get("messages").cloned();
    properties["$ai_input"] = input_messages.clone().unwrap_or(Value::Null);
    if let Some(output) = response.get("choices").or_else(|| response.get("content")) {
        properties["$ai_output_choices"] = transform_output_choices(output);
    }
    let max_tokens = request_body.get("max_tokens");
    if max_tokens.is_some() {
        properties["$ai_model_parameters"] = json!({"max_tokens": max_tokens.unwrap()});
    }

    // One $ai_trace per trace id, emitted ahead of the first generation event.
    if let Some(trace_id) = current_trace_id() {
        let mut seen = ph.seen_traces.lock().unwrap();
        if seen.len() >= SEEN_TRACES_CAP {
            seen.clear();
        }
        if seen.insert(trace_id.clone()) {
            drop(seen);
            capture(
                "$ai_trace",
                json!({
                    "$ai_trace_id": trace_id,
                    "$ai_trace_name": extract_user_message(input_messages.as_ref()),
                    "$ai_provider": provider,
                    "$ai_latency": latency.as_secs_f64(),
                }),
            );
        }
    }

    capture("$ai_generation", properties);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn require_fails_closed_outside_scope() {
        assert!(require_current_user_id().is_err());
    }

    #[tokio::test]
    async fn scoped_user_id_is_readable() {
        let id = scoped(Some("u-1".to_string()), async {
            require_current_user_id().unwrap()
        })
        .await;
        assert_eq!(id, "u-1");
        assert!(require_current_user_id().is_err());
    }

    #[test]
    fn extract_user_message_reads_openai_and_pydantic_shapes() {
        let openai = json!([{"role":"user","content":"hello"}]);
        assert_eq!(extract_user_message(Some(&openai)), "hello");
        let pydantic = json!([{"role":"user","parts":[{"type":"text","content":"hi"}]}]);
        assert_eq!(extract_user_message(Some(&pydantic)), "hi");
        assert_eq!(extract_user_message(None), "");
    }
}
