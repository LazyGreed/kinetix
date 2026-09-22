//! Inbound frontends (FR-1, FR-2). Each frontend decodes a client request into
//! the internal model and encodes internal streaming events back into that
//! format's exact SSE wire shape.

pub mod anthropic;
pub mod models;
pub mod openai;
pub mod responses;

use bytes::Bytes;
use serde_json::Value;

use crate::types::{FinishReason, InternalRequest, Message, Part, ProxyError, StreamEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendFormat {
    OpenAi,
    Anthropic,
    OpenAiResponses,
}

const TRANSLATION_ISSUES_KEY: &str = "__kinetix_translation_issues";

pub(crate) fn attach_translation_issues(
    extra: &mut serde_json::Map<String, Value>,
    issues: Vec<String>,
) {
    if !issues.is_empty() {
        extra.insert(
            TRANSLATION_ISSUES_KEY.to_string(),
            Value::Array(issues.into_iter().map(Value::String).collect()),
        );
    }
}

/// Resolve tool-result names from previous assistant tool calls. Some inbound
/// formats only repeat the call id on the result, while Gemini requires the
/// original function name on its function-response object.
pub(crate) fn resolve_tool_result_names(messages: &mut [Message]) -> Vec<String> {
    let mut names = std::collections::HashMap::<String, String>::new();
    let mut issues = Vec::new();

    for (message_index, message) in messages.iter_mut().enumerate() {
        for part in &message.parts {
            if let Part::ToolCall {
                id: Some(id), name, ..
            } = part
            {
                names.insert(id.clone(), name.clone());
            }
        }

        for part in &mut message.parts {
            if let Part::ToolResult {
                tool_call_id, name, ..
            } = part
            {
                if name.is_none() {
                    if let Some(resolved) = names.get(tool_call_id) {
                        *name = Some(resolved.clone());
                    } else if tool_call_id.is_empty() {
                        issues.push(format!(
                            "messages[{message_index}] tool result is missing tool-call identity"
                        ));
                    } else {
                        issues.push(format!(
                            "messages[{message_index}] tool result references unknown tool call id '{tool_call_id}'"
                        ));
                    }
                }
            }
        }
    }

    issues
}

impl FrontendFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            FrontendFormat::OpenAi => "openai",
            FrontendFormat::Anthropic => "anthropic",
            FrontendFormat::OpenAiResponses => "openai-responses",
        }
    }
}

/// Context passed to an encoder for a single response.
pub struct EncoderCtx {
    pub model_name: String,
    pub request_id: String,
    pub created: i64,
}

/// Decode a client request body into the internal request model.
pub fn decode(format: FrontendFormat, body: Value) -> Result<InternalRequest, ProxyError> {
    match format {
        FrontendFormat::OpenAi => openai::decode_request(body),
        FrontendFormat::Anthropic => anthropic::decode_request(body),
        FrontendFormat::OpenAiResponses => responses::decode_request(body),
    }
}

/// Per-request streaming encoder. Holds the small amount of state each wire
/// format needs (open blocks, tool indices, whether the header was sent).
pub enum Encoder {
    OpenAi(openai::OpenAiEncoder),
    Anthropic(anthropic::AnthropicEncoder),
    Responses(responses::ResponsesEncoder),
}

impl Encoder {
    pub fn new(format: FrontendFormat, ctx: EncoderCtx) -> Self {
        match format {
            FrontendFormat::OpenAi => Encoder::OpenAi(openai::OpenAiEncoder::new(ctx)),
            FrontendFormat::Anthropic => Encoder::Anthropic(anthropic::AnthropicEncoder::new(ctx)),
            FrontendFormat::OpenAiResponses => {
                Encoder::Responses(responses::ResponsesEncoder::new(ctx))
            }
        }
    }

    pub fn set_include_usage(&mut self, include_usage: bool) {
        if let Encoder::OpenAi(encoder) = self {
            encoder.set_include_usage(include_usage);
        }
    }

    /// Encode one internal event into zero or more SSE frames.
    pub fn encode(&mut self, event: StreamEvent) -> Vec<Bytes> {
        match self {
            Encoder::OpenAi(e) => e.encode(event),
            Encoder::Anthropic(e) => e.encode(event),
            Encoder::Responses(e) => e.encode(event),
        }
    }

    /// Produce the terminator frame(s) after the upstream stream ends.
    pub fn finalize(&mut self) -> Vec<Bytes> {
        match self {
            Encoder::OpenAi(e) => e.finalize(),
            Encoder::Anthropic(e) => e.finalize(),
            Encoder::Responses(e) => e.finalize(),
        }
    }

    /// Encode a mid-stream error in a format-native way (FR-4.5 / open issue).
    pub fn error_frame(&mut self, message: &str) -> Vec<Bytes> {
        match self {
            Encoder::OpenAi(e) => e.error_frame(message),
            Encoder::Anthropic(e) => e.error_frame(message),
            Encoder::Responses(e) => e.error_frame(message),
        }
    }
}

/// A single SSE frame helper: `event: X\ndata: {...}\n\n` (event name optional).
pub fn sse_frame(event: Option<&str>, data: &str) -> Bytes {
    let mut s = String::with_capacity(data.len() + 32);
    if let Some(ev) = event {
        s.push_str("event: ");
        s.push_str(ev);
        s.push('\n');
    }
    for line in data.split('\n') {
        s.push_str("data: ");
        s.push_str(line);
        s.push('\n');
    }
    s.push('\n');
    Bytes::from(s)
}

pub fn sse_comment(text: &str) -> Bytes {
    Bytes::from(format!(": {text}\n\n"))
}

/// Aggregate a full event stream into a single non-streaming response body in
/// the calling frontend's format (FR-1.4). One code path: the upstream stream
/// is consumed and aggregated rather than re-requested non-streaming.
pub fn aggregate(
    format: FrontendFormat,
    model_name: &str,
    request_id: &str,
    events: Vec<StreamEvent>,
    usage: &crate::types::TokenUsage,
) -> Value {
    match format {
        FrontendFormat::OpenAi => {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut tool_calls: Vec<(u32, String, String, String)> = Vec::new();
            let mut finish = FinishReason::Stop;
            for ev in events {
                match ev {
                    StreamEvent::TextDelta(t) => text.push_str(&t),
                    StreamEvent::ThinkingDelta { text: t, .. } => reasoning.push_str(&t),
                    StreamEvent::ToolCallStart {
                        index, id, name, ..
                    } => tool_calls.push((
                        index,
                        id.unwrap_or_else(|| format!("call_{}_{}", request_id, index)),
                        name,
                        String::new(),
                    )),
                    StreamEvent::ToolCallArgsDelta { index, args } => {
                        if let Some(tc) = tool_calls.iter_mut().find(|t| t.0 == index) {
                            tc.3.push_str(&args);
                        }
                    }
                    StreamEvent::Finish(f) => finish = f,
                    _ => {}
                }
            }
            let mut message = serde_json::json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::String(text) }
            });
            if !reasoning.is_empty() {
                message["reasoning_content"] = Value::String(reasoning);
            }
            if !tool_calls.is_empty() {
                let tcs: Vec<Value> = tool_calls
                    .iter()
                    .map(|(_, id, name, args)| {
                        serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": if args.is_empty() { "{}" } else { args } }
                        })
                    })
                    .collect();
                message["tool_calls"] = Value::Array(tcs);
            }
            let mut usage_obj = serde_json::json!({
                "prompt_tokens": usage.input.unwrap_or(0),
                "completion_tokens": usage.output.unwrap_or(0),
                "total_tokens": usage.input.unwrap_or(0) + usage.output.unwrap_or(0),
            });
            // Unknown means unknown (FR-6.2/6.8): only emit detail fields the
            // upstream actually reported, never a coerced zero.
            if let Some(c) = usage.cached {
                usage_obj["prompt_tokens_details"] = serde_json::json!({ "cached_tokens": c });
            }
            if let Some(t) = usage.thinking {
                usage_obj["completion_tokens_details"] =
                    serde_json::json!({ "reasoning_tokens": t });
            }
            serde_json::json!({
                "id": format!("chatcmpl-{}", request_id.replace(['-','_'], "")),
                "object": "chat.completion",
                "created": chrono::Utc::now().timestamp(),
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": openai_finish_str(&finish)
                }],
                "usage": usage_obj
            })
        }
        FrontendFormat::Anthropic => {
            let mut blocks: Vec<Value> = Vec::new();
            let mut cur_tool: Option<(u32, String, String, String)> = None;
            let mut finish = FinishReason::Stop;
            let push_tool = |cur: &mut Option<(u32, String, String, String)>,
                             blocks: &mut Vec<Value>| {
                if let Some((_, id, name, args)) = cur.take() {
                    let input: Value = serde_json::from_str(&args).unwrap_or(serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type": "tool_use", "id": id, "name": name, "input": input
                    }));
                }
            };
            for ev in events {
                match ev {
                    StreamEvent::TextDelta(t) => {
                        push_tool(&mut cur_tool, &mut blocks);
                        blocks.push(serde_json::json!({"type": "text", "text": t}));
                    }
                    StreamEvent::ThinkingDelta { text, signature } => {
                        blocks.push(serde_json::json!({
                            "type": "thinking", "thinking": text,
                            "signature": signature.unwrap_or_default()
                        }));
                    }
                    StreamEvent::ToolCallStart {
                        index, id, name, ..
                    } => {
                        push_tool(&mut cur_tool, &mut blocks);
                        cur_tool = Some((
                            index,
                            id.unwrap_or_else(|| format!("toolu_{}_{}", request_id, index)),
                            name,
                            String::new(),
                        ));
                    }
                    StreamEvent::ToolCallArgsDelta { args, .. } => {
                        if let Some(cur) = cur_tool.as_mut() {
                            cur.3.push_str(&args);
                        }
                    }
                    StreamEvent::Finish(f) => finish = f,
                    _ => {}
                }
            }
            push_tool(&mut cur_tool, &mut blocks);
            let stop_reason = match finish {
                FinishReason::Length => "max_tokens",
                FinishReason::ToolCalls => "tool_use",
                _ => "end_turn",
            };
            // FR-6.2/6.8: input/output are the required fields (0 when the
            // upstream omitted them); cached tokens are omitted when unknown
            // rather than coerced to zero.
            let mut usage_obj = serde_json::json!({
                "input_tokens": usage.input.unwrap_or(0),
                "output_tokens": usage.output.unwrap_or(0),
            });
            if let Some(c) = usage.cached {
                usage_obj["cache_read_input_tokens"] = serde_json::json!(c);
            }
            serde_json::json!({
                "id": format!("msg_{}", request_id.replace(['-','_'], "")),
                "type": "message",
                "role": "assistant",
                "model": model_name,
                "content": blocks,
                "stop_reason": stop_reason,
                "stop_sequence": Value::Null,
                "usage": usage_obj
            })
        }
        FrontendFormat::OpenAiResponses => {
            responses::aggregate_responses(model_name, request_id, events, usage)
        }
    }
}

fn openai_finish_str(f: &FinishReason) -> &str {
    match f {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Other(s) => s.as_str(),
    }
}

/// Reject behaviorally significant client fields that the translation path
/// cannot faithfully carry (FR-2.8: never silently drop behaviorally significant
/// client fields). Same-format passthrough preserves these verbatim, so this is
/// only consulted when translation is required.
///
/// Cosmetic/unknown fields are ignored rather than rejected.
pub fn translation_unsupported(
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if let Some(issue) = extra
        .get(TRANSLATION_ISSUES_KEY)
        .and_then(Value::as_array)
        .and_then(|issues| issues.first())
        .and_then(Value::as_str)
    {
        return Some(format!(
            "unsupported nested content cannot be translated safely: {issue}"
        ));
    }
    // Fields that materially change the meaning of a completion and have no
    // faithful translation into the other supported wire formats.
    if let Some(n) = extra.get("n").and_then(|v| v.as_u64()) {
        if n > 1 {
            return Some(
                "'n' (multiple completions) is not supported on a translating path; \
                 remove it or route to a same-format provider"
                    .to_string(),
            );
        }
    }
    if extra.contains_key("logprobs") || extra.contains_key("top_logprobs") {
        return Some("'logprobs'/'top_logprobs' are not supported on a translating path".into());
    }
    if let Some(rf) = extra.get("response_format") {
        // A plain text/json_object hint is safe to drop; a json_schema constraint
        // is not enforceable through the other formats.
        let has_schema = rf.get("json_schema").map(|v| !v.is_null()).unwrap_or(false)
            || rf.get("type").and_then(|v| v.as_str()) == Some("json_schema");
        if has_schema {
            return Some(
                "'response_format.json_schema' is not enforceable on a translating path".into(),
            );
        }
    }
    if extra.contains_key("modalities") || extra.contains_key("audio") {
        return Some("'modalities'/'audio' output are not supported on a translating path".into());
    }
    if extra.contains_key("prediction") {
        return Some("'prediction' is not supported on a translating path".into());
    }
    None
}
