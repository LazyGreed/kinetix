//! Same-format passthrough (FR-2.7, FR-2.10).
//!
//! When the inbound frontend's wire format matches the selected provider's
//! outbound wire format, Kinetix forwards the client's protocol content with
//! minimal parsing/rewriting instead of translating through the internal model.
//! This preserves unknown/provider-specific fields (opaque state) that the
//! internal model does not represent.
//!
//! What is still applied on the passthrough path (FR-2.7): authentication,
//! routing, cancellation, accounting, safety checks, and — for accounting —
//! best-effort extraction of *usage metadata only*.
//!
//! The only content rewrite is the `model` field, replaced with the selected
//! upstream model id (the client sent an alias/Route/model name).

use serde_json::Value;

use crate::frontends::FrontendFormat;
use crate::types::WireFormat;

/// Whether an inbound/outbound format pair qualifies for passthrough.
pub fn is_passthrough(inbound: FrontendFormat, outbound: WireFormat) -> bool {
    match inbound {
        FrontendFormat::OpenAi => outbound == WireFormat::Openai,
        FrontendFormat::Anthropic => outbound == WireFormat::Anthropic,
        FrontendFormat::OpenAiResponses => false,
    }
}

/// Rewrite only the `model` field of a client body to the upstream model id,
/// preserving every other field byte-for-byte in structure.
///
/// When `force_stream` is set (the non-streaming aggregation path), `stream` is
/// also set to `true`: Kinetix always consumes the upstream as a stream
/// internally and aggregates when the client asked for a single JSON body. This
/// is required for OpenAI-compatible upstreams, which only emit the terminal
/// usage chunk when streaming (FR-1.4 + FR-6.2).
pub fn rewrite_model(
    raw: &str,
    upstream_id: &str,
    force_stream: bool,
    force_usage: bool,
    wire: WireFormat,
) -> Option<String> {
    let mut v: Value = serde_json::from_str(raw).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert("model".to_string(), Value::String(upstream_id.to_string()));
    if force_stream {
        obj.insert("stream".to_string(), Value::Bool(true));
    }
    // OpenAI-compatible upstream usage is an internal accounting concern.
    // Deep-set include_usage=true even when the client explicitly sent false;
    // the response path filters that internal override from client output.
    if force_usage && wire == WireFormat::Openai {
        let stream_options = obj
            .entry("stream_options".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !stream_options.is_object() {
            *stream_options = serde_json::json!({});
        }
        stream_options
            .as_object_mut()
            .unwrap()
            .insert("include_usage".to_string(), Value::Bool(true));
    }
    serde_json::to_string(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn passthrough_pairs() {
        assert!(is_passthrough(FrontendFormat::OpenAi, WireFormat::Openai));
        assert!(is_passthrough(
            FrontendFormat::Anthropic,
            WireFormat::Anthropic
        ));
        assert!(!is_passthrough(FrontendFormat::OpenAi, WireFormat::Gemini));
        assert!(!is_passthrough(FrontendFormat::OpenAi, WireFormat::Plugin));
        assert!(!is_passthrough(
            FrontendFormat::OpenAiResponses,
            WireFormat::Openai
        ));
    }

    #[test]
    fn rewrite_keeps_unknown_fields() {
        let raw = json!({
            "model": "coder",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "vendor_extension": {"keep": [1, 2, 3]}
        })
        .to_string();
        let out =
            rewrite_model(&raw, "gemini-3.6-flash", false, false, WireFormat::Openai).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "gemini-3.6-flash");
        assert_eq!(v["vendor_extension"]["keep"][2], 3);
    }

    #[test]
    fn force_stream_sets_stream_and_usage() {
        let raw = json!({
            "model": "free",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string();
        let out = rewrite_model(&raw, "mimo-v2.5-free", true, true, WireFormat::Openai).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
    }
    #[test]
    fn force_usage_overrides_explicit_false_without_changing_client_intent() {
        let raw = json!({
            "model": "free",
            "stream": true,
            "stream_options": { "include_usage": false, "other": "keep" },
            "messages": []
        })
        .to_string();
        let out = rewrite_model(&raw, "upstream", false, true, WireFormat::Openai).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["stream_options"]["other"], "keep");
    }

    #[test]
    fn anthropic_force_stream_does_not_add_openai_stream_options() {
        let raw = json!({
            "model": "claude",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string();
        let out =
            rewrite_model(&raw, "claude-upstream", true, true, WireFormat::Anthropic).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["stream"], true);
        assert!(v.get("stream_options").is_none());
    }
}
