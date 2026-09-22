//! Host-side [`Adapter`] backed by a plugin (`plugin-adapter` world, §6.3, §7.1).
//!
//! The plugin adapter is a *pure translation library*: Kinetix core still owns
//! the outbound streaming HTTP send and the SSE framing, and calls this adapter
//! only for synchronous transforms (`build_url`/`apply_auth`/`build_body`) and
//! response parsing (`classify_error`/`parse_stream_chunk`/`parse_full_response`).
//! The `plugin-adapter` world therefore imports no network capability at all.
//!
//! Because the [`Adapter`] trait is synchronous while guest calls are async, the
//! bridge uses `tokio::task::block_in_place` + `Handle::block_on`. This is sound
//! only on the multi-threaded runtime (Kinetix's server); under a current-thread
//! runtime (some unit tests) `block_in_place` panics, so callers must build the
//! adapter on a multi-thread runtime.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{Adapter, DiscoveredModel, UpstreamContext};
use crate::plugins::manager::PluginManager;
use crate::plugins::runtime::PluginFault;
use crate::types::{
    FailureKind, FinishReason, ImageData, InternalRequest, Part, ProxyError, StreamEvent,
    TokenUsage, UpstreamFailure,
};

/// A plugin-backed adapter bound to one `plugin:<id>/<capability>` reference.
pub struct PluginAdapter {
    manager: PluginManager,
    plugin_id: String,
    /// The wire-format name the guest reported (cached at construction).
    wire_format: &'static str,
}

impl PluginAdapter {
    /// Probe the plugin's adapter world for its wire-format name, returning an
    /// error (so registration can fail closed) if the guest cannot answer.
    pub async fn new(manager: PluginManager, plugin_id: String) -> anyhow::Result<Self> {
        let wf = manager
            .adapter_wire_format(&plugin_id)
            .await
            .map_err(|e| anyhow::anyhow!("plugin adapter wire_format: {}", e.message()))?;
        let wire_format: &'static str = Box::leak(wf.into_boxed_str());
        Ok(PluginAdapter {
            manager,
            plugin_id,
            wire_format,
        })
    }

    /// Run a guest adapter call from a synchronous context, bridging the async
    /// guest invocation onto the current multi-threaded runtime.
    fn block<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, PluginFault>>,
    ) -> Result<T, PluginFault> {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    fn provider_json(ctx: &UpstreamContext<'_>) -> String {
        serde_json::to_string(ctx.provider).unwrap_or_else(|_| "{}".to_string())
    }

    fn model_json(ctx: &UpstreamContext<'_>) -> String {
        serde_json::to_string(ctx.model).unwrap_or_else(|_| "{}".to_string())
    }

    fn plugin_err(e: PluginFault) -> ProxyError {
        ProxyError::upstream(format!("plugin adapter: {}", e.message()))
    }
}

#[async_trait]
impl Adapter for PluginAdapter {
    fn wire_format(&self) -> &'static str {
        self.wire_format
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        let (p, m) = (Self::provider_json(ctx), Self::model_json(ctx));
        self.block(self.manager.adapter_build_url(&self.plugin_id, &p, &m))
            .map_err(Self::plugin_err)
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        let p = Self::provider_json(ctx);
        // The credential has already been resolved by core and is passed to the
        // guest so it can build the header list (never leaked to the client).
        let credential = ctx.credential.clone();
        let headers_json = match self.block(self.manager.adapter_apply_auth(
            &self.plugin_id,
            &p,
            &credential,
        )) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(plugin = %self.plugin_id, error = %e.message(), "plugin adapter apply_auth failed");
                return req;
            }
        };
        let headers: Vec<(String, String)> =
            serde_json::from_str(&headers_json).unwrap_or_default();
        let mut req = req;
        for (name, value) in headers {
            req = req.header(name, value);
        }
        req
    }

    fn build_body(&self, ctx: &UpstreamContext<'_>, req: &InternalRequest) -> Value {
        let request_json = request_to_json(req);
        let (p, m) = (Self::provider_json(ctx), Self::model_json(ctx));
        match self.block(
            self.manager
                .adapter_build_body(&self.plugin_id, &request_json, &p, &m),
        ) {
            Ok(body) => serde_json::from_str(&body).unwrap_or(Value::Null),
            Err(e) => {
                tracing::warn!(plugin = %self.plugin_id, error = %e.message(), "plugin adapter build_body failed");
                Value::Null
            }
        }
    }

    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        let headers_json = headers_to_json(headers);
        match self.block(self.manager.adapter_classify_error(
            &self.plugin_id,
            status,
            body,
            &headers_json,
        )) {
            Ok(ev) => json_to_failure(&ev).unwrap_or_else(|| fallback_failure(status)),
            Err(_) => fallback_failure(status),
        }
    }

    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let out = self
            .block(
                self.manager
                    .adapter_parse_stream_chunk(&self.plugin_id, data),
            )
            .map_err(|e| UpstreamFailure {
                kind: FailureKind::ServerError,
                status: None,
                retry_after_secs: None,
                message: format!("plugin adapter parse_stream_chunk: {}", e.message()),
                quota_reset_at: None,
            })?;
        json_to_events(&out)
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let body_json = serde_json::to_string(body).unwrap_or_default();
        let out = self
            .block(
                self.manager
                    .adapter_parse_full_response(&self.plugin_id, &body_json),
            )
            .map_err(|e| UpstreamFailure {
                kind: FailureKind::ServerError,
                status: None,
                retry_after_secs: None,
                message: format!("plugin adapter parse_full_response: {}", e.message()),
                quota_reset_at: None,
            })?;
        json_to_events(&out)
    }

    fn default_models_path(&self) -> &'static str {
        // Model discovery is a separate capability (`model-source`); the plugin
        // adapter does not own the discovery path.
        "/models"
    }

    fn parse_model_list(&self, _body: &Value) -> Vec<DiscoveredModel> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Canonical JSON encoding across the host boundary
// ---------------------------------------------------------------------------

/// Serialize an internal request into the JSON the guest adapter consumes.
/// `raw_body`/`extra` carry provider-specific fields; the model/params are
/// modelled explicitly.
pub fn request_to_json(req: &InternalRequest) -> String {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let parts: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => json!({ "type": "text", "text": t }),
                    Part::Image(ImageData::Base64 { mime, data }) => {
                        json!({ "type": "image", "mime": mime, "data": data })
                    }
                    Part::Image(ImageData::Url(url)) => json!({ "type": "image_url", "url": url }),
                    Part::ToolCall {
                        id,
                        name,
                        arguments,
                        signature,
                    } => json!({
                        "type": "tool_call", "id": id, "name": name,
                        "arguments": arguments, "signature": signature
                    }),
                    Part::ToolResult {
                        tool_call_id,
                        name,
                        content,
                        is_error,
                    } => json!({
                        "type": "tool_result", "tool_call_id": tool_call_id,
                        "name": name, "content": content, "is_error": is_error
                    }),
                    Part::Thinking { text, signature } => {
                        json!({ "type": "thinking", "text": text, "signature": signature })
                    }
                })
                .collect();
            json!({ "role": format!("{:?}", m.role).to_lowercase(), "parts": parts })
        })
        .collect();
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(
            |t| json!({ "name": t.name, "description": t.description, "parameters": t.parameters }),
        )
        .collect();
    let value = json!({
        "requested_model": req.requested_model,
        "system": req.system,
        "messages": messages,
        "tools": tools,
        "tool_choice": req.tool_choice_name,
        "stream": req.stream,
        "temperature": req.params.temperature,
        "top_p": req.params.top_p,
        "top_k": req.params.top_k,
        "max_tokens": req.params.max_tokens,
        "stop": req.params.stop,
        "seed": req.params.seed,
        "extra": req.extra,
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn headers_to_json(headers: &reqwest::header::HeaderMap) -> String {
    let mut map = serde_json::Map::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            map.insert(name.as_str().to_string(), json!(v));
        }
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
}

/// Encode canonical stream events as a JSON array tagged by `type`.
pub fn events_to_json(events: &[StreamEvent]) -> String {
    let arr: Vec<Value> = events.iter().map(event_to_value).collect();
    serde_json::to_string(&Value::Array(arr)).unwrap_or_else(|_| "[]".to_string())
}

fn event_to_value(ev: &StreamEvent) -> Value {
    match ev {
        StreamEvent::Start {
            upstream_request_id,
        } => json!({ "type": "start", "upstream_request_id": upstream_request_id }),
        StreamEvent::ThinkingDelta { text, signature } => {
            json!({ "type": "thinking_delta", "text": text, "signature": signature })
        }
        StreamEvent::TextDelta(t) => json!({ "type": "text_delta", "text": t }),
        StreamEvent::ToolCallStart {
            index,
            id,
            name,
            signature,
        } => json!({
            "type": "tool_call_start", "index": index, "id": id,
            "name": name, "signature": signature
        }),
        StreamEvent::ToolCallArgsDelta { index, args } => {
            json!({ "type": "tool_call_args_delta", "index": index, "args": args })
        }
        StreamEvent::Usage(u) => json!({
            "type": "usage", "input": u.input, "output": u.output,
            "cached": u.cached, "thinking": u.thinking
        }),
        StreamEvent::Finish(r) => json!({ "type": "finish", "reason": r.as_str() }),
    }
}

/// Decode a canonical stream-event JSON array from the guest.
pub fn json_to_events(s: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
    let value: Value = serde_json::from_str(s).map_err(|e| UpstreamFailure {
        kind: FailureKind::ServerError,
        status: None,
        retry_after_secs: None,
        message: format!("plugin adapter returned invalid event JSON: {e}"),
        quota_reset_at: None,
    })?;
    let arr = value.as_array().ok_or_else(|| UpstreamFailure {
        kind: FailureKind::ServerError,
        status: None,
        retry_after_secs: None,
        message: "plugin adapter event payload was not an array".to_string(),
        quota_reset_at: None,
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        out.push(value_to_event(v)?);
    }
    Ok(out)
}

fn value_to_event(v: &Value) -> Result<StreamEvent, UpstreamFailure> {
    let bad = |m: &str| UpstreamFailure {
        kind: FailureKind::ServerError,
        status: None,
        retry_after_secs: None,
        message: format!("plugin adapter event: {m}"),
        quota_reset_at: None,
    };
    let ty = v
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| bad("missing type"))?;
    Ok(match ty {
        "start" => StreamEvent::Start {
            upstream_request_id: v
                .get("upstream_request_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        },
        "thinking_delta" => StreamEvent::ThinkingDelta {
            text: v
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            signature: v
                .get("signature")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        },
        "text_delta" => StreamEvent::TextDelta(
            v.get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        "tool_call_start" => StreamEvent::ToolCallStart {
            index: v.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            id: v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()),
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            signature: v
                .get("signature")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        },
        "tool_call_args_delta" => StreamEvent::ToolCallArgsDelta {
            index: v.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            args: v
                .get("args")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "usage" => StreamEvent::Usage(TokenUsage {
            input: v.get("input").and_then(|x| x.as_u64()),
            output: v.get("output").and_then(|x| x.as_u64()),
            cached: v.get("cached").and_then(|x| x.as_u64()),
            thinking: v.get("thinking").and_then(|x| x.as_u64()),
        }),
        "finish" => StreamEvent::Finish(match v.get("reason").and_then(|x| x.as_str()) {
            Some("stop") => FinishReason::Stop,
            Some("length") => FinishReason::Length,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("content_filter") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_string()),
            None => FinishReason::Stop,
        }),
        other => return Err(bad(&format!("unknown event type '{other}'"))),
    })
}

/// Encode classified failure evidence for the guest.
pub fn failure_to_json(f: &UpstreamFailure) -> String {
    let value = json!({
        "kind": failure_kind_str(f.kind),
        "status": f.status,
        "retry_after_secs": f.retry_after_secs,
        "message": f.message,
        "quota_reset_at": f.quota_reset_at.map(|d| d.to_rfc3339()),
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Decode classified failure evidence from the guest.
pub fn json_to_failure(s: &str) -> Option<UpstreamFailure> {
    let v: Value = serde_json::from_str(s).ok()?;
    let kind = match v.get("kind").and_then(|x| x.as_str())? {
        "rate_limit" => FailureKind::RateLimit,
        "quota_exhausted" => FailureKind::QuotaExhausted,
        "auth_error" => FailureKind::AuthError,
        "target_error" => FailureKind::TargetError,
        "server_error" => FailureKind::ServerError,
        "connection_error" => FailureKind::ConnectionError,
        "timeout" => FailureKind::Timeout,
        "bad_request" => FailureKind::BadRequest,
        _ => FailureKind::ServerError,
    };
    Some(UpstreamFailure {
        kind,
        status: v.get("status").and_then(|x| x.as_u64()).map(|x| x as u16),
        retry_after_secs: v.get("retry_after_secs").and_then(|x| x.as_u64()),
        message: v
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("upstream error")
            .to_string(),
        quota_reset_at: v
            .get("quota_reset_at")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc)),
    })
}

fn failure_kind_str(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::RateLimit => "rate_limit",
        FailureKind::QuotaExhausted => "quota_exhausted",
        FailureKind::AuthError => "auth_error",
        FailureKind::TargetError => "target_error",
        FailureKind::ServerError => "server_error",
        FailureKind::ConnectionError => "connection_error",
        FailureKind::Timeout => "timeout",
        FailureKind::BadRequest => "bad_request",
    }
}

/// Status-only classification used when the guest cannot be reached.
fn fallback_failure(status: u16) -> UpstreamFailure {
    let kind = match status {
        401 => FailureKind::AuthError,
        403 | 404 => FailureKind::TargetError,
        429 => FailureKind::RateLimit,
        400..=499 => FailureKind::BadRequest,
        _ => FailureKind::ServerError,
    };
    UpstreamFailure {
        kind,
        status: Some(status),
        retry_after_secs: None,
        message: format!("upstream returned {status}"),
        quota_reset_at: None,
    }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<PluginAdapter>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenUsage;

    #[test]
    fn stream_events_round_trip() {
        let events = vec![
            StreamEvent::Start {
                upstream_request_id: Some("req_1".into()),
            },
            StreamEvent::ThinkingDelta {
                text: "hmm".into(),
                signature: Some("sig".into()),
            },
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::ToolCallStart {
                index: 2,
                id: Some("call_1".into()),
                name: "fn".into(),
                signature: None,
            },
            StreamEvent::ToolCallArgsDelta {
                index: 2,
                args: "{}".into(),
            },
            StreamEvent::Usage(TokenUsage {
                input: Some(10),
                output: Some(4),
                cached: None,
                thinking: Some(2),
            }),
            StreamEvent::Finish(FinishReason::ToolCalls),
        ];
        let json = events_to_json(&events);
        let back = json_to_events(&json).expect("decode");
        assert_eq!(back.len(), events.len());
        match &back[0] {
            StreamEvent::Start {
                upstream_request_id,
            } => assert_eq!(upstream_request_id.as_deref(), Some("req_1")),
            _ => panic!("expected start"),
        }
        match &back[5] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, Some(10));
                assert_eq!(u.thinking, Some(2));
            }
            _ => panic!("expected usage"),
        }
        assert!(matches!(
            back[6],
            StreamEvent::Finish(FinishReason::ToolCalls)
        ));
    }

    #[test]
    fn invalid_event_json_is_rejected() {
        assert!(json_to_events("not json").is_err());
        assert!(json_to_events("{\"type\":\"bogus\"}").is_err());
        // A bare object (not an array) is rejected.
        assert!(json_to_events("{\"type\":\"text_delta\"}").is_err());
    }

    #[test]
    fn failure_evidence_round_trip() {
        let f = UpstreamFailure {
            kind: FailureKind::RateLimit,
            status: Some(429),
            retry_after_secs: Some(7),
            message: "slow down".into(),
            quota_reset_at: None,
        };
        let back = json_to_failure(&failure_to_json(&f)).expect("decode");
        assert_eq!(back.kind, FailureKind::RateLimit);
        assert_eq!(back.status, Some(429));
        assert_eq!(back.retry_after_secs, Some(7));
        assert_eq!(back.message, "slow down");
    }

    #[test]
    fn fallback_classification_maps_status() {
        assert_eq!(fallback_failure(401).kind, FailureKind::AuthError);
        assert_eq!(fallback_failure(429).kind, FailureKind::RateLimit);
        assert_eq!(fallback_failure(400).kind, FailureKind::BadRequest);
        assert_eq!(fallback_failure(503).kind, FailureKind::ServerError);
    }

    #[test]
    fn request_json_carries_core_facts_only() {
        use crate::types::{InternalRequest, Message, Part, Role};
        let req = InternalRequest {
            requested_model: "gemini-3".into(),
            system: vec!["sys".into()],
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text("hello".into())],
            }],
            tools: Vec::new(),
            tool_choice: None,
            tool_choice_name: None,
            params: Default::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        };
        let v: Value = serde_json::from_str(&request_to_json(&req)).unwrap();
        assert_eq!(v["requested_model"], "gemini-3");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["parts"][0]["type"], "text");
    }
}
