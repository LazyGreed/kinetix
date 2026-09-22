//! Gemini `generateContent` outbound adapter.
//!
//! Chosen purely by the provider's configured `wire_format = "gemini"`; nothing
//! vendor-specific leaks into the core. URLs, auth scheme, and headers all come
//! from the provider config the admin entered.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{Adapter, DiscoveredModel, UpstreamContext};
use crate::types::{
    FailureKind, FinishReason, ImageData, InternalRequest, Message, Part, ProxyError, Role,
    SamplingParams, StreamEvent, TokenUsage, ToolChoice, UpstreamFailure,
};

pub struct GeminiAdapter;

impl GeminiAdapter {
    pub fn new() -> Self {
        GeminiAdapter
    }

    fn role_str(role: Role) -> &'static str {
        match role {
            Role::Assistant => "model",
            // System is hoisted to systemInstruction; tool results ride as user parts.
            Role::System | Role::User | Role::Tool => "user",
        }
    }

    fn encode_parts(parts: &[Part], out: &mut Vec<Value>) {
        for p in parts {
            match p {
                Part::Text(t) => out.push(json!({ "text": t })),
                Part::Thinking { text, .. } => {
                    // Preserve thinking as a thought part so multi-turn history
                    // stays valid for the same provider (FR-12.10).
                    if !text.is_empty() {
                        out.push(json!({ "text": text, "thought": true }));
                    }
                }
                Part::Image(img) => match img {
                    ImageData::Base64 { mime, data } => {
                        out.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
                    }
                    ImageData::Url(url) => {
                        out.push(json!({ "fileData": { "fileUri": url } }));
                    }
                },
                Part::ToolCall {
                    name,
                    arguments,
                    signature,
                    ..
                } => {
                    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
                    let mut part = json!({ "functionCall": { "name": name, "args": args } });
                    if let Some(sig) = signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    out.push(part);
                }
                Part::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let name = name.clone().unwrap_or_else(|| "tool".to_string());
                    // Gemini expects a structured response object.
                    let response = if *is_error {
                        json!({ "error": content })
                    } else {
                        match serde_json::from_str::<Value>(content) {
                            Ok(Value::Object(_)) => serde_json::from_str(content).unwrap(),
                            _ => json!({ "result": content }),
                        }
                    };
                    out.push(json!({
                        "functionResponse": { "name": name, "response": response }
                    }));
                }
            }
        }
    }

    fn build_contents(req: &InternalRequest) -> Vec<Value> {
        let mut contents = Vec::new();
        for m in &req.messages {
            let mut parts = Vec::new();
            Self::encode_parts(&m.parts, &mut parts);
            if parts.is_empty() {
                continue;
            }
            contents.push(json!({ "role": Self::role_str(m.role), "parts": parts }));
        }
        contents
    }

    fn build_generation_config(ctx: &UpstreamContext<'_>, req: &InternalRequest) -> Value {
        let model = ctx.model;
        let params = model.params();
        let mut cfg = serde_json::Map::new();

        let put_number = |policy_key: &str,
                          wire_key: &str,
                          client_val: Option<f64>,
                          cfg: &mut serde_json::Map<String, Value>| {
            let spec = params.get(policy_key);
            if let Some(v) = client_val {
                let (value, keep) = apply_param_spec(spec, v);
                if keep {
                    cfg.insert(wire_key.to_string(), json!(value));
                }
            } else if let Some(spec) = spec {
                // FR-10.6: policy lookup always uses canonical Kinetix names;
                // only the emitted wire key is provider-specific.
                if spec.supported {
                    if let Some(d) = spec.default {
                        cfg.insert(wire_key.to_string(), json!(d));
                    }
                }
            }
        };

        put_number("temperature", "temperature", req.params.temperature, &mut cfg);
        put_number("top_p", "topP", req.params.top_p, &mut cfg);
        put_number("top_k", "topK", req.params.top_k, &mut cfg);

        // max output tokens: clamp to model max if configured.
        if let Some(mt) = req.params.max_tokens {
            let mut v = mt as f64;
            if let Some(max) = model.max_output_tokens {
                v = v.min(max as f64);
            }
            cfg.insert("maxOutputTokens".to_string(), json!(v as i64));
        }
        if !req.params.stop.is_empty() {
            cfg.insert("stopSequences".to_string(), json!(req.params.stop));
        }
        if let Some(seed) = req.params.seed {
            cfg.insert("seed".to_string(), json!(seed));
        }

        // Thinking mapping (FR-10.7): canonical level -> upstream fields.
        if let Some(level) = req.thinking {
            let tmap = model.thinking();
            let key = match level {
                crate::types::ThinkingLevel::Off => "off",
                crate::types::ThinkingLevel::Low => "low",
                crate::types::ThinkingLevel::Medium => "medium",
                crate::types::ThinkingLevel::High => "high",
            };
            if let Some(v) = tmap.levels.get(key) {
                // Value may be a full object to merge, or a scalar under budget_field.
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        insert_dotted(&mut cfg, k, val.clone());
                    }
                } else if let Some(field) = &tmap.budget_field {
                    insert_dotted(&mut cfg, field, v.clone());
                }
            }
        }

        Value::Object(cfg)
    }

    fn build_tools(req: &InternalRequest) -> Option<Value> {
        if req.tools.is_empty() {
            return None;
        }
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut d = json!({ "name": t.name });
                if let Some(desc) = &t.description {
                    d["description"] = json!(desc);
                }
                if !t.parameters.is_null() {
                    d["parameters"] = sanitize_schema(&t.parameters);
                }
                d
            })
            .collect();
        Some(json!([{ "functionDeclarations": decls }]))
    }

    fn build_tool_config(req: &InternalRequest) -> Option<Value> {
        let mode = match req.tool_choice {
            Some(ToolChoice::Auto) | None => {
                if req.tools.is_empty() {
                    return None;
                }
                "AUTO"
            }
            Some(ToolChoice::None) => "NONE",
            Some(ToolChoice::Required) => "ANY",
            Some(ToolChoice::Specific) => "ANY",
        };
        let mut cfg = json!({ "mode": mode });
        if let Some(name) = &req.tool_choice_name {
            cfg["allowedFunctionNames"] = json!([name]);
        }
        Some(json!({ "functionCallingConfig": cfg }))
    }
}

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply the admin's parameter policy (FR-10.6).
/// Returns (value, keep). `drop`/out-of-range reject handled by caller via error.
fn apply_param_spec(spec: Option<&crate::types::ParamSpec>, v: f64) -> (f64, bool) {
    let Some(spec) = spec else {
        // Unconfigured parameter: forward unchanged.
        return (v, true);
    };
    if !spec.supported {
        return (v, false); // drop
    }
    let mut value = v;
    if let Some(min) = spec.min {
        if value < min {
            value = min;
        }
    }
    if let Some(max) = spec.max {
        if value > max {
            value = max;
        }
    }
    (value, true)
}

/// Insert a value at a dotted path (`a.b.c`) inside a JSON object, creating
/// intermediate objects as needed.
fn insert_dotted(obj: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    insert_rec(obj, &parts, value);
}

fn insert_rec(obj: &mut serde_json::Map<String, Value>, path: &[&str], value: Value) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        obj.insert(path[0].to_string(), value);
        return;
    }
    let entry = obj.entry(path[0].to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    if let Some(map) = entry.as_object_mut() {
        insert_rec(map, &path[1..], value);
    }
}

/// Resolve local JSON-Schema references before removing definition containers.
/// Gemini does not accept `$ref`, so stripping `$defs` first would leave
/// dangling references and silently weaken tool validation.
fn resolve_local_refs(schema: &Value) -> Value {
    fn resolve(node: &Value, root: &Value, stack: &mut Vec<String>) -> Value {
        match node {
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                    if reference.starts_with("#/") {
                        if stack.iter().any(|seen| seen == reference) {
                            // Recursive schemas cannot be represented faithfully
                            // in Gemini's inline subset. Stop expansion safely
                            // rather than recursing forever or forwarding $ref.
                            return json!({});
                        }
                        if let Some(target) = root.pointer(&reference[1..]) {
                            stack.push(reference.to_string());
                            let mut resolved = resolve(target, root, stack);
                            stack.pop();

                            // Modern JSON Schema permits siblings alongside $ref.
                            // Preserve them by overlaying the resolved object.
                            if let Some(out) = resolved.as_object_mut() {
                                for (key, value) in map {
                                    if key != "$ref" {
                                        out.insert(key.clone(), resolve(value, root, stack));
                                    }
                                }
                                return resolved;
                            }
                            if map.len() == 1 {
                                return resolved;
                            }
                        }
                    }
                }
                Value::Object(
                    map.iter()
                        .map(|(key, value)| (key.clone(), resolve(value, root, stack)))
                        .collect(),
                )
            }
            Value::Array(values) => {
                Value::Array(values.iter().map(|value| resolve(value, root, stack)).collect())
            }
            other => other.clone(),
        }
    }

    resolve(schema, schema, &mut Vec::new())
}

fn merge_all_of(branches: &[Value]) -> Value {
    let mut out = serde_json::Map::new();
    for branch in branches {
        let Value::Object(map) = branch else {
            continue;
        };
        for (key, value) in map {
            match (out.get_mut(key), value) {
                (Some(Value::Object(existing)), Value::Object(incoming))
                    if key == "properties" =>
                {
                    for (property, schema) in incoming {
                        existing.insert(property.clone(), schema.clone());
                    }
                }
                (Some(Value::Array(existing)), Value::Array(incoming)) if key == "required" => {
                    for value in incoming {
                        if !existing.contains(value) {
                            existing.push(value.clone());
                        }
                    }
                }
                (None, _) => {
                    out.insert(key.clone(), value.clone());
                }
                _ => {}
            }
        }
    }
    Value::Object(out)
}

fn normalize_type_array(
    values: &[Value],
    source: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let mut out = source.clone();
    let nullable = values.iter().any(|value| value.as_str() == Some("null"));
    let non_null: Vec<&str> = values
        .iter()
        .filter_map(Value::as_str)
        .filter(|value| *value != "null")
        .collect();
    out.remove("type");

    if nullable {
        out.insert("nullable".to_string(), Value::Bool(true));
    }
    if non_null.len() == 1 {
        out.insert("type".to_string(), json!(non_null[0]));
    } else if !non_null.is_empty() {
        out.insert(
            "anyOf".to_string(),
            Value::Array(
                non_null
                    .iter()
                    .map(|kind| json!({ "type": kind }))
                    .collect(),
            ),
        );
    }
    out
}

/// Normalize JSON Schema to the subset accepted by Gemini.
///
/// Local references are resolved first; then unsupported syntax is converted
/// without leaving dangling references. `oneOf` becomes Gemini-supported
/// `anyOf`, compatible `allOf` object branches are merged, nullable type
/// unions become `nullable`, tuple items become an `anyOf` item schema, and
/// `required` is filtered to surviving properties.
fn sanitize_schema(schema: &Value) -> Value {
    fn sanitize(node: &Value) -> Value {
        match node {
            Value::Object(original) => {
                let normalized_type;
                let map = if let Some(types) = original.get("type").and_then(Value::as_array) {
                    normalized_type = normalize_type_array(types, original);
                    &normalized_type
                } else {
                    original
                };

                let mut out = serde_json::Map::new();
                for (key, value) in map {
                    match key.as_str() {
                        "additionalProperties"
                        | "$schema"
                        | "definitions"
                        | "$defs"
                        | "$ref"
                        | "strict"
                        | "prefixItems" => {}
                        "const" => {
                            out.insert("enum".to_string(), json!([sanitize(value)]));
                        }
                        "oneOf" => {
                            if let Some(branches) = value.as_array() {
                                out.insert(
                                    "anyOf".to_string(),
                                    Value::Array(branches.iter().map(sanitize).collect()),
                                );
                            }
                        }
                        "allOf" => {
                            if let Some(branches) = value.as_array() {
                                let sanitized: Vec<Value> =
                                    branches.iter().map(sanitize).collect();
                                let merged = merge_all_of(&sanitized);
                                if let Some(merged_obj) = merged.as_object() {
                                    for (merged_key, merged_value) in merged_obj {
                                        out.insert(merged_key.clone(), merged_value.clone());
                                    }
                                }
                            }
                        }
                        "items" if value.is_array() => {
                            let branches = value.as_array().unwrap();
                            out.insert(
                                "items".to_string(),
                                json!({
                                    "anyOf": branches.iter().map(sanitize).collect::<Vec<_>>()
                                }),
                            );
                        }
                        _ => {
                            out.insert(key.clone(), sanitize(value));
                        }
                    }
                }

                if let Some(required) = out.get("required").and_then(Value::as_array).cloned() {
                    if let Some(properties) = out.get("properties").and_then(Value::as_object) {
                        let filtered: Vec<Value> = required
                            .into_iter()
                            .filter(|value| {
                                value
                                    .as_str()
                                    .map(|name| properties.contains_key(name))
                                    .unwrap_or(false)
                            })
                            .collect();
                        if filtered.is_empty() {
                            out.remove("required");
                        } else {
                            out.insert("required".to_string(), Value::Array(filtered));
                        }
                    }
                }

                Value::Object(out)
            }
            Value::Array(values) => Value::Array(values.iter().map(sanitize).collect()),
            other => other.clone(),
        }
    }

    sanitize(&resolve_local_refs(schema))
}

#[async_trait]
impl Adapter for GeminiAdapter {
    fn wire_format(&self) -> &'static str {
        "gemini"
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        let base = ctx.provider.base_url.trim_end_matches('/');
        // Streaming-first: Kinetix always consumes an upstream stream (the
        // non-streaming path aggregates it), so always use the streaming method.
        Ok(format!(
            "{base}/models/{}:streamGenerateContent?alt=sse",
            ctx.model.upstream_id
        ))
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        use crate::types::AuthScheme;
        match ctx.provider.auth() {
            AuthScheme::Bearer => req.bearer_auth(&ctx.credential),
            AuthScheme::CustomHeader => {
                let name = ctx
                    .provider
                    .custom_header_name
                    .clone()
                    .unwrap_or_else(|| "x-goog-api-key".to_string());
                req.header(name, &ctx.credential)
            }
            AuthScheme::QueryParam => {
                let param = ctx
                    .provider
                    .custom_param_name
                    .clone()
                    .unwrap_or_else(|| "key".to_string());
                req.query(&[(param, &ctx.credential)])
            }
        }
    }

    fn build_body(&self, ctx: &UpstreamContext<'_>, req: &InternalRequest) -> Value {
        let mut body = serde_json::Map::new();

        // System instruction.
        if !req.system.is_empty() {
            let text = req.system.join("\n\n");
            body.insert(
                "systemInstruction".to_string(),
                json!({ "parts": [{ "text": text }] }),
            );
        }

        body.insert("contents".to_string(), json!(Self::build_contents(req)));

        let gen_cfg = Self::build_generation_config(ctx, req);
        if gen_cfg.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            body.insert("generationConfig".to_string(), gen_cfg);
        }

        if let Some(tools) = Self::build_tools(req) {
            body.insert("tools".to_string(), tools);
        }
        if let Some(tc) = Self::build_tool_config(req) {
            body.insert("toolConfig".to_string(), tc);
        }

        // Merge admin extra request fields (FR-10.8).
        let extra = ctx.model.extra_request_value();
        merge_extra(&mut body, &extra);

        Value::Object(body)
    }

    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let message = parsed
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let gstatus = parsed
            .pointer("/error/status")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let retry_after_secs = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
            .or_else(|| parse_retry_delay(&parsed));

        // A short retry hint (Gemini's `retryDelay` for a per-minute rate limit)
        // means the window rolls over quickly, so treat it as a rate limit even
        // when the message mentions "quota" (e.g. free-tier RPM caps). A long
        // hint indicates a daily/quota reset and stays an exhaustion.
        let short_retry = retry_after_secs.map(|s| s <= 90).unwrap_or(false);

        let lower = message.to_ascii_lowercase();
        let credential_error = status == 401
            || gstatus == "UNAUTHENTICATED"
            || lower.contains("api key not valid")
            || lower.contains("invalid api key")
            || lower.contains("invalid authentication credentials");

        let kind = match status {
            400 if credential_error => FailureKind::AuthError,
            400 | 422 => FailureKind::BadRequest,
            401 => FailureKind::AuthError,
            403 if credential_error => FailureKind::AuthError,
            403 | 404 => FailureKind::TargetError,
            429 if short_retry => FailureKind::RateLimit,
            429 => classify_429(gstatus, &message),
            s if s >= 500 => FailureKind::ServerError,
            _ => FailureKind::ServerError,
        };

        let safe_message = if message.is_empty() {
            format!("upstream returned HTTP {status}")
        } else {
            crate::crypto::redact(&message)
        };

        UpstreamFailure {
            kind,
            status: Some(status),
            retry_after_secs,
            message: safe_message,
            quota_reset_at: None,
        }
    }

    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let v: Value = serde_json::from_str(data).map_err(|e| UpstreamFailure {
            kind: FailureKind::ServerError,
            status: None,
            retry_after_secs: None,
            message: format!("invalid upstream chunk: {e}"),
            quota_reset_at: None,
        })?;
        Ok(events_from_gemini(&v))
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(events_from_gemini(body))
    }

    fn default_models_path(&self) -> &'static str {
        "/models"
    }

    fn parse_model_list(&self, body: &Value) -> Vec<DiscoveredModel> {
        let mut out = Vec::new();
        if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
            for m in models {
                let Some(name) = m.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let id = name.strip_prefix("models/").unwrap_or(name).to_string();
                // Only surface models that can actually generate content.
                let methods = m
                    .get("supportedGenerationMethods")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default();
                if !methods.is_empty() && !methods.iter().any(|x| x.contains("generateContent")) {
                    continue;
                }
                out.push(DiscoveredModel {
                    id,
                    display_name: m
                        .get("displayName")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    context_window: m.get("inputTokenLimit").and_then(|v| v.as_i64()),
                    max_output_tokens: m.get("outputTokenLimit").and_then(|v| v.as_i64()),
                });
            }
        }
        out
    }
}

/// Convert a Gemini chunk/response into internal stream events.
fn events_from_gemini(v: &Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut tool_index = 0u32;

    if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) {
        for cand in candidates {
            if let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        let is_thought = part
                            .get("thought")
                            .and_then(|t| t.as_bool())
                            .unwrap_or(false);
                        let signature = part
                            .get("thoughtSignature")
                            .and_then(|s| s.as_str())
                            .map(String::from);
                        if is_thought {
                            events.push(StreamEvent::ThinkingDelta {
                                text: text.to_string(),
                                signature,
                            });
                        } else if !text.is_empty() {
                            events.push(StreamEvent::TextDelta(text.to_string()));
                        } else if signature.is_some() {
                            // Thought-signature-only part.
                            events.push(StreamEvent::ThinkingDelta {
                                text: String::new(),
                                signature,
                            });
                        }
                    }
                    if let Some(fc) = part.get("functionCall") {
                        let name = fc
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let id = fc.get("id").and_then(|i| i.as_str()).map(String::from);
                        let args = fc.get("args").cloned().unwrap_or(json!({}));
                        let signature = part
                            .get("thoughtSignature")
                            .and_then(|s| s.as_str())
                            .map(String::from);
                        events.push(StreamEvent::ToolCallStart {
                            index: tool_index,
                            id,
                            name,
                            signature,
                        });
                        events.push(StreamEvent::ToolCallArgsDelta {
                            index: tool_index,
                            args: args.to_string(),
                        });
                        tool_index += 1;
                    }
                }
            }
            if let Some(reason) = cand.get("finishReason").and_then(|r| r.as_str()) {
                events.push(StreamEvent::Finish(map_finish(reason)));
            }
        }
    }

    if let Some(usage) = v.get("usageMetadata") {
        events.push(StreamEvent::Usage(TokenUsage {
            input: usage.get("promptTokenCount").and_then(|v| v.as_u64()),
            output: usage.get("candidatesTokenCount").and_then(|v| v.as_u64()),
            cached: usage
                .get("cachedContentTokenCount")
                .and_then(|v| v.as_u64()),
            thinking: usage.get("thoughtsTokenCount").and_then(|v| v.as_u64()),
        }));
    }

    events
}

fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            FinishReason::ContentFilter
        }
        other => FinishReason::Other(other.to_string()),
    }
}

fn classify_429(status: &str, message: &str) -> FailureKind {
    let lower = message.to_lowercase();
    let quota_like = lower.contains("quota")
        || lower.contains("exceeded your current quota")
        || lower.contains("billing")
        || status == "RESOURCE_EXHAUSTED" && lower.contains("free_tier");
    // Rate limits are short-lived; quota exhaustion resets on a schedule.
    if quota_like
        && (lower.contains("per_day") || lower.contains("per day") || lower.contains("free_tier"))
    {
        FailureKind::QuotaExhausted
    } else if lower.contains("rate limit") || lower.contains("requests per minute") {
        FailureKind::RateLimit
    } else if quota_like {
        FailureKind::QuotaExhausted
    } else {
        FailureKind::RateLimit
    }
}

fn parse_retry_after(value: &str) -> Option<u64> {
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(secs);
    }
    // HTTP-date form.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(value) {
        let delta = dt.with_timezone(&chrono::Utc) - chrono::Utc::now();
        return Some(delta.num_seconds().max(0) as u64);
    }
    None
}

/// Gemini puts `"retryDelay": "23s"` inside error details.
fn parse_retry_delay(v: &Value) -> Option<u64> {
    let details = v.pointer("/error/details")?.as_array()?;
    for d in details {
        if let Some(delay) = d.get("retryDelay").and_then(|x| x.as_str()) {
            let num = delay.trim_end_matches('s').parse::<f64>().ok()?;
            return Some(num.ceil() as u64);
        }
    }
    None
}

fn merge_extra(body: &mut serde_json::Map<String, Value>, extra: &Value) {
    let Some(obj) = extra.as_object() else { return };
    for (k, v) in obj {
        // set-if-absent semantics by default.
        if !body.contains_key(k) {
            body.insert(k.clone(), v.clone());
        } else if let (Some(existing), Some(new)) = (
            body.get_mut(k).and_then(|x| x.as_object_mut()),
            v.as_object(),
        ) {
            for (k2, v2) in new {
                existing.entry(k2.clone()).or_insert_with(|| v2.clone());
            }
        }
    }
}

/// Re-export for the tool-result name lookup in the translator.
pub fn sampling_defaults() -> SamplingParams {
    SamplingParams::default()
}

pub fn message_has_tool_result(m: &Message) -> bool {
    m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn sanitize_schema_resolves_nested_local_refs_before_stripping_defs() {
        let input = json!({
            "$ref": "#/$defs/Envelope",
            "$defs": {
                "Envelope": {
                    "type": "object",
                    "required": ["payload", "removed"],
                    "properties": {
                        "payload": { "$ref": "#/$defs/Payload" }
                    }
                },
                "Payload": {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "ok" }
                    },
                    "required": ["kind"]
                }
            }
        });
        let got = sanitize_schema(&input);
        assert!(got.get("$ref").is_none());
        assert!(got.get("$defs").is_none());
        assert_eq!(
            got.pointer("/properties/payload/properties/kind/enum"),
            Some(&json!(["ok"]))
        );
        assert_eq!(got["required"], json!(["payload"]));
    }

    #[test]
    fn sanitize_schema_normalizes_unions_and_tuple_items() {
        let input = json!({
            "type": "object",
            "properties": {
                "value": { "type": ["string", "null"] },
                "choice": { "oneOf": [{"type":"string"}, {"type":"number"}] },
                "tuple": { "type":"array", "items":[{"type":"string"},{"type":"integer"}] }
            }
        });
        let got = sanitize_schema(&input);
        assert_eq!(got.pointer("/properties/value/type"), Some(&json!("string")));
        assert_eq!(got.pointer("/properties/value/nullable"), Some(&json!(true)));
        assert!(got.pointer("/properties/choice/oneOf").is_none());
        assert_eq!(
            got.pointer("/properties/choice/anyOf/1/type"),
            Some(&json!("number"))
        );
        assert_eq!(
            got.pointer("/properties/tuple/items/anyOf/0/type"),
            Some(&json!("string"))
        );
    }

    #[test]
    fn sanitize_schema_recurses_and_preserves_const_semantics() {
        let input = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "config": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "mode": { "const": "fast" },
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": { "kind": { "const": "x" } }
                            }
                        }
                    }
                }
            }
        });
        let got = sanitize_schema(&input);
        assert!(got.get("$schema").is_none());
        assert!(got.get("additionalProperties").is_none());
        assert_eq!(
            got.pointer("/properties/config/properties/mode/enum"),
            Some(&json!(["fast"]))
        );
        assert!(got
            .pointer("/properties/config/additionalProperties")
            .is_none());
        assert!(got
            .pointer("/properties/config/properties/items/items/additionalProperties")
            .is_none());
        assert_eq!(
            got.pointer("/properties/config/properties/items/items/properties/kind/enum"),
            Some(&json!(["x"]))
        );
    }
}

#[cfg(test)]
mod error_scope_tests {
    use super::*;
    use crate::adapters::Adapter;

    #[test]
    fn permission_denied_does_not_poison_gemini_credential() {
        let adapter = GeminiAdapter::new();
        let forbidden = adapter.classify_error(
            403,
            r#"{"error":{"status":"PERMISSION_DENIED","message":"project is not allowed to use this model"}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(forbidden.kind, FailureKind::TargetError);

        let invalid_key = adapter.classify_error(
            400,
            r#"{"error":{"status":"INVALID_ARGUMENT","message":"API key not valid. Please pass a valid API key."}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(invalid_key.kind, FailureKind::AuthError);
    }
}
