//! Outbound adapter interface (FR-11.1). One adapter per wire format; chosen by
//! the provider's configured wire format, never by vendor. Built-in code goes
//! through exactly this interface so a plugin can attach later without rewrites.

use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

pub mod anthropic;
pub mod gemini;
pub mod openai;

use crate::types::{InternalRequest, ProxyError, StreamEvent, UpstreamFailure, WireFormat};

/// Everything an adapter needs to build and authenticate one upstream call.
pub struct UpstreamContext<'a> {
    pub provider: &'a crate::db::ProviderRow,
    pub model: &'a crate::db::ModelRow,
    /// Selected account identity. This is non-secret context for account-scoped
    /// plugin state; built-in adapters do not use it.
    pub account_id: Option<&'a str>,
    /// The decrypted credential for the chosen account.
    pub credential: String,
}

/// The result of a successful (accepted) upstream call.
pub struct UpstreamStream {
    pub upstream_request_id: Option<String>,
    pub events: BoxStream<'static, Result<StreamEvent, UpstreamFailure>>,
}

#[async_trait]
pub trait Adapter: Send + Sync {
    /// Wire format this adapter speaks.
    fn wire_format(&self) -> &'static str;

    /// Build the outbound URL for a model call.
    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError>;

    /// Whether this adapter owns translation/validation of canonical thinking
    /// levels. Built-in adapters rely on an executable core ThinkingMap;
    /// plugin adapters must explicitly opt in through their manifest.
    fn handles_thinking_translation(&self) -> bool {
        false
    }

    /// Whether this adapter provides an exact upstream token-count API.
    fn supports_count_tokens(&self) -> bool {
        false
    }

    /// Optional exact token-count endpoint for this wire adapter. Returning
    /// `None` means Kinetix must use its documented local estimate.
    fn count_tokens_url(&self, _ctx: &UpstreamContext<'_>) -> Result<Option<String>, ProxyError> {
        Ok(None)
    }

    /// Apply authentication to a request builder (header or query param).
    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure>;

    /// Translate the internal request into the upstream JSON body.
    fn build_body(
        &self,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
    ) -> Result<serde_json::Value, UpstreamFailure>;

    /// Apply model-aware policy to a same-format passthrough body before dispatch.
    /// Most adapters preserve passthrough content unchanged.
    fn normalize_passthrough_body(
        &self,
        _ctx: &UpstreamContext<'_>,
        _req: &InternalRequest,
        _body: &mut serde_json::Value,
    ) -> Result<(), UpstreamFailure> {
        Ok(())
    }

    /// Parse an upstream non-2xx response into a classified failure.
    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure;

    /// Parse one SSE `data:` payload (or one JSON object) into stream events.
    /// Returning an empty vec is fine (e.g. keepalive or metadata-only chunk).
    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure>;

    /// Parse a full non-streaming response body into events (thin fallback path).
    fn parse_full_response(
        &self,
        body: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, UpstreamFailure>;

    /// The path used for model discovery.
    fn default_models_path(&self) -> &'static str {
        "/models"
    }

    /// Extract model IDs (and suggested limits) from a discovery response.
    fn parse_model_list(&self, body: &serde_json::Value) -> Vec<DiscoveredModel> {
        let _ = body;
        Vec::new()
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningCapabilityMode {
    Toggle,
    Level,
    ManualBudget,
    Adaptive,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ReasoningCapability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ReasoningCapabilityMode>,
    pub levels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub can_disable: bool,
    pub upstream_format: String,
    #[serde(skip_serializing)]
    upstream_levels: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCapabilityFlags {
    pub text: Option<bool>,
    pub reasoning: Option<bool>,
    pub vision: Option<bool>,
    pub tool_calling: Option<bool>,
    pub structured_output: Option<bool>,
}

fn canonical_reasoning_levels(
    value: &serde_json::Value,
) -> (Vec<String>, std::collections::HashMap<String, String>) {
    let Some(values) = value.as_array() else {
        return (Vec::new(), std::collections::HashMap::new());
    };
    let mut levels = Vec::new();
    let mut upstream_levels = std::collections::HashMap::new();
    for value in values {
        let Some(raw) = value.as_str() else {
            continue;
        };
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => "off",
            "minimal" => "minimal",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" | "x-high" | "x_high" | "extra_high" => "xhigh",
            "max" => "max",
            _ => continue,
        };
        if !levels.iter().any(|level| level == normalized) {
            levels.push(normalized.to_string());
            upstream_levels.insert(normalized.to_string(), raw.to_string());
        }
    }
    (levels, upstream_levels)
}

fn reasoning_capability(
    levels: Vec<String>,
    upstream_levels: std::collections::HashMap<String, String>,
    mode: ReasoningCapabilityMode,
    upstream_format: impl Into<String>,
    can_disable: Option<bool>,
) -> Option<ReasoningCapability> {
    if levels.is_empty() {
        return None;
    }
    let inferred_disable = levels.iter().any(|level| level == "off");
    Some(ReasoningCapability {
        mode: Some(mode),
        levels,
        default: None,
        can_disable: can_disable.unwrap_or(inferred_disable),
        upstream_format: upstream_format.into(),
        upstream_levels,
    })
}

pub fn reasoning_metadata_declared(metadata: &serde_json::Value) -> bool {
    if metadata.get("reasoning_capability").is_some() {
        return true;
    }
    if metadata
        .get("reasoning")
        .is_some_and(serde_json::Value::is_boolean)
    {
        return true;
    }
    if metadata
        .get("reasoning")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|reasoning| {
            reasoning.contains_key("supported")
                || reasoning.contains_key("mode")
                || reasoning.contains_key("levels")
        })
    {
        return true;
    }
    [
        "/supportedThinkingEfforts",
        "/reasoning/supported_efforts",
        "/supported_reasoning_levels",
        "/thinking/levels",
        "/capabilities/effort_tiers",
    ]
    .iter()
    .any(|pointer| metadata.pointer(pointer).is_some())
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCapabilitiesV1 {
    schema_version: u32,
    transport: Option<TransportCapabilityV1>,
    text: Option<SupportCapabilityV1>,
    reasoning: Option<PluginReasoningCapabilityV1>,
    tools: Option<SupportCapabilityV1>,
    vision: Option<VisionCapabilityV1>,
    structured_output: Option<SupportCapabilityV1>,
    /// Canonical plugin-supplied pricing. Kept in the existing v1 JSON
    /// envelope so old WIT components remain ABI-compatible.
    #[allow(dead_code)]
    prices: Option<serde_json::Value>,
    /// Optional directional modality metadata from plugin discovery.
    #[allow(dead_code)]
    modalities: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TransportCapabilityV1 {
    format: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SupportCapabilityV1 {
    supported: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VisionCapabilityV1 {
    input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum PluginReasoningModeV1 {
    Toggle,
    Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
enum PluginReasoningLevelV1 {
    #[serde(rename = "minimal")]
    Minimal,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    #[serde(rename = "max")]
    Max,
}

impl PluginReasoningLevelV1 {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginReasoningCapabilityV1 {
    supported: bool,
    mode: Option<PluginReasoningModeV1>,
    levels: Option<Vec<PluginReasoningLevelV1>>,
    default: Option<PluginReasoningLevelV1>,
    can_disable: Option<bool>,
}

impl PluginReasoningCapabilityV1 {
    fn is_valid(&self) -> bool {
        if !self.supported {
            return self.mode.is_none()
                && self.levels.is_none()
                && self.default.is_none()
                && self.can_disable.is_none();
        }

        match self.mode {
            None => self.levels.is_none() && self.default.is_none(),
            Some(PluginReasoningModeV1::Toggle) => self.levels.is_none() && self.default.is_none(),
            Some(PluginReasoningModeV1::Level) => {
                let Some(levels) = self.levels.as_ref() else {
                    return false;
                };
                if levels.is_empty() {
                    return false;
                }
                let unique: std::collections::HashSet<_> = levels.iter().copied().collect();
                if unique.len() != levels.len() {
                    return false;
                }
                self.default
                    .map(|default| levels.contains(&default))
                    .unwrap_or(true)
            }
        }
    }
}

impl ModelCapabilitiesV1 {
    fn is_valid(&self) -> bool {
        if self.schema_version != 1 {
            return false;
        }
        if self
            .transport
            .as_ref()
            .is_some_and(|transport| transport.format.trim().is_empty())
        {
            return false;
        }
        self.reasoning
            .as_ref()
            .map(PluginReasoningCapabilityV1::is_valid)
            .unwrap_or(true)
    }
}

fn parse_model_capabilities_v1(metadata: &serde_json::Value) -> Option<ModelCapabilitiesV1> {
    let metadata: ModelCapabilitiesV1 = serde_json::from_value(metadata.clone()).ok()?;
    metadata.is_valid().then_some(metadata)
}

pub fn plugin_capability_flags_v1(metadata: &serde_json::Value) -> Option<ModelCapabilityFlags> {
    let metadata = parse_model_capabilities_v1(metadata)?;
    Some(ModelCapabilityFlags {
        text: metadata.text.as_ref().map(|text| text.supported),
        reasoning: metadata
            .reasoning
            .as_ref()
            .map(|reasoning| reasoning.supported),
        vision: metadata.vision.as_ref().map(|vision| vision.input),
        tool_calling: metadata.tools.as_ref().map(|tools| tools.supported),
        structured_output: metadata
            .structured_output
            .as_ref()
            .map(|structured| structured.supported),
    })
}

pub fn plugin_reasoning_support_v1(metadata: &serde_json::Value) -> Option<bool> {
    parse_model_capabilities_v1(metadata)?
        .reasoning
        .map(|reasoning| reasoning.supported)
}

pub fn normalize_plugin_reasoning_capability_v1(
    metadata: &serde_json::Value,
) -> Option<ReasoningCapability> {
    let metadata = parse_model_capabilities_v1(metadata)?;
    let reasoning = metadata.reasoning?;
    if !reasoning.supported {
        return None;
    }

    let mode = match reasoning.mode {
        None => None,
        Some(PluginReasoningModeV1::Toggle) => Some(ReasoningCapabilityMode::Toggle),
        Some(PluginReasoningModeV1::Level) => Some(ReasoningCapabilityMode::Level),
    };
    let levels: Vec<String> = reasoning
        .levels
        .unwrap_or_default()
        .into_iter()
        .map(|level| level.as_str().to_string())
        .collect();
    let default = reasoning.default.map(|level| level.as_str().to_string());
    let upstream_format = match metadata
        .transport
        .as_ref()
        .map(|transport| transport.format.as_str())
    {
        Some("openai") => "openai_effort",
        Some("openai-responses") => "responses_effort",
        Some("gemini") => "gemini_thinking_level",
        _ => "provider_declared",
    };
    let upstream_levels = levels
        .iter()
        .map(|level| (level.clone(), level.clone()))
        .collect();

    Some(ReasoningCapability {
        mode,
        levels,
        default,
        can_disable: reasoning.can_disable.unwrap_or(false),
        upstream_format: upstream_format.to_string(),
        upstream_levels,
    })
}

/// Normalize provider-native/legacy discovery metadata into Kinetix's canonical
/// reasoning capability. Versioned plugin capabilities_json uses the strict v1
/// parser above. Unknown provider levels are discarded rather than invented.
pub fn normalize_reasoning_capability(metadata: &serde_json::Value) -> Option<ReasoningCapability> {
    // Provider metadata may already expose a normalized-looking shape under
    // reasoning or reasoning_capability.
    let normalized = metadata.get("reasoning_capability").or_else(|| {
        metadata.get("reasoning").filter(|value| {
            value.as_object().is_some_and(|reasoning| {
                reasoning.contains_key("supported")
                    || reasoning.contains_key("mode")
                    || reasoning.contains_key("levels")
            })
        })
    });
    if let Some(value) = normalized {
        if value.get("supported").and_then(serde_json::Value::as_bool) == Some(false) {
            return None;
        }

        let (levels, mut upstream_levels) =
            canonical_reasoning_levels(value.get("levels").unwrap_or(&serde_json::Value::Null));
        let mode = match value.get("mode").and_then(serde_json::Value::as_str) {
            Some("toggle") => Some(ReasoningCapabilityMode::Toggle),
            Some("level") => Some(ReasoningCapabilityMode::Level),
            Some("manual_budget") => Some(ReasoningCapabilityMode::ManualBudget),
            Some("adaptive") => Some(ReasoningCapabilityMode::Adaptive),
            Some(_) => return None,
            None if levels.is_empty() => None,
            None => Some(ReasoningCapabilityMode::Level),
        };
        if matches!(mode, Some(ReasoningCapabilityMode::Toggle)) && !levels.is_empty() {
            return None;
        }
        if matches!(
            mode,
            Some(
                ReasoningCapabilityMode::Level
                    | ReasoningCapabilityMode::ManualBudget
                    | ReasoningCapabilityMode::Adaptive
            )
        ) && levels.is_empty()
        {
            return None;
        }

        let upstream_format = value
            .get("upstream_format")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("provider_declared");

        // Normalized plugin metadata contains canonical levels, so aliases are
        // no longer recoverable from the level strings themselves. Apply
        // dialect defaults for known formats, then allow plugins to provide an
        // explicit canonical -> upstream token map for provider-specific aliases.
        if matches!(upstream_format, "openai_effort" | "responses_effort")
            && levels.iter().any(|level| level == "off")
        {
            upstream_levels.insert("off".to_string(), "none".to_string());
        }
        if let Some(explicit) = value
            .get("upstream_levels")
            .and_then(serde_json::Value::as_object)
        {
            for (canonical, upstream) in explicit {
                if levels.iter().any(|level| level == canonical) {
                    if let Some(upstream) = upstream.as_str() {
                        upstream_levels.insert(canonical.clone(), upstream.to_string());
                    }
                }
            }
        }

        let inferred_disable = levels.iter().any(|level| level == "off");
        return Some(ReasoningCapability {
            mode,
            levels,
            default: None,
            can_disable: value
                .get("can_disable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(inferred_disable),
            upstream_format: upstream_format.to_string(),
            upstream_levels,
        });
    }

    let candidates = [
        (
            "/supportedThinkingEfforts",
            "provider_supported_thinking_efforts",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/reasoning/supported_efforts",
            "provider_reasoning_supported_efforts",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/supported_reasoning_levels",
            "provider_supported_reasoning_levels",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/thinking/levels",
            "provider_thinking_levels",
            ReasoningCapabilityMode::Level,
        ),
        (
            "/capabilities/effort_tiers",
            "provider_effort_tiers",
            ReasoningCapabilityMode::Level,
        ),
    ];
    for (pointer, upstream_format, mode) in candidates {
        let Some(value) = metadata.pointer(pointer) else {
            continue;
        };
        let (levels, upstream_levels) = canonical_reasoning_levels(value);
        return reasoning_capability(levels, upstream_levels, mode, upstream_format, None);
    }
    None
}

/// Derive an executable legacy ThinkingMap only when the discovered dialect
/// unambiguously identifies the upstream effort field. Other normalized
/// capabilities stay descriptive until an adapter/registry supplies a mapping.
pub fn thinking_map_for_reasoning(
    capability: &ReasoningCapability,
) -> Option<crate::types::ThinkingMap> {
    if capability.mode != Some(ReasoningCapabilityMode::Level) {
        return None;
    }
    let level_field = match capability.upstream_format.as_str() {
        "openai_effort" => "reasoning_effort",
        // Responses transport is descriptive until runtime model transport
        // selection can dispatch it through a Responses-capable adapter.
        "responses_effort" => return None,
        _ => return None,
    };
    let mut levels: std::collections::HashMap<String, serde_json::Value> = capability
        .levels
        .iter()
        .map(|level| {
            let upstream = capability
                .upstream_levels
                .get(level)
                .cloned()
                .unwrap_or_else(|| level.clone());
            (level.clone(), serde_json::Value::String(upstream))
        })
        .collect();
    if capability.can_disable && capability.upstream_format == "openai_effort" {
        levels
            .entry("off".to_string())
            .or_insert_with(|| serde_json::Value::String("none".to_string()));
    }
    Some(crate::types::ThinkingMap {
        levels,
        mode: Some(crate::types::ThinkingMode::Level),
        budget_field: None,
        level_field: Some(level_field.to_string()),
    })
}

pub fn thinking_map_for_reasoning_with_wire(
    capability: &ReasoningCapability,
    wire: crate::types::WireFormat,
) -> Option<crate::types::ThinkingMap> {
    if capability.mode != Some(ReasoningCapabilityMode::Level) {
        return None;
    }

    let level_field = match (wire, capability.upstream_format.as_str()) {
        (
            crate::types::WireFormat::Openai,
            "openai_effort"
            | "provider_supported_thinking_efforts"
            | "provider_supported_reasoning_levels",
        ) => "reasoning_effort",
        (crate::types::WireFormat::Gemini, "gemini_thinking_level") => {
            "thinkingConfig.thinkingLevel"
        }
        // Per-model Responses transport is descriptive until runtime adapter
        // selection can actually dispatch this model through the Responses API.
        (crate::types::WireFormat::Openai, "responses_effort") => return None,
        _ => return None,
    };

    let mut levels: std::collections::HashMap<String, serde_json::Value> = capability
        .levels
        .iter()
        .map(|level| {
            let upstream = capability
                .upstream_levels
                .get(level)
                .cloned()
                .unwrap_or_else(|| level.clone());
            (level.clone(), serde_json::Value::String(upstream))
        })
        .collect();
    if capability.can_disable && capability.upstream_format == "openai_effort" {
        levels
            .entry("off".to_string())
            .or_insert_with(|| serde_json::Value::String("none".to_string()));
    }
    Some(crate::types::ThinkingMap {
        levels,
        mode: Some(crate::types::ThinkingMode::Level),
        budget_field: None,
        level_field: Some(level_field.to_string()),
    })
}

/// An adapter registry: selects the built-in adapter for a wire format.
/// Adding a new wire format (or a plugin) means adding an entry here; no
/// frontend code changes (NFR-5.1).
#[derive(Clone)]
pub struct AdapterRegistry {
    gemini: Arc<dyn Adapter>,
    openai: Arc<dyn Adapter>,
    anthropic: Arc<dyn Adapter>,
    /// Plugin-host-backed adapters keyed by `plugin:<id>/<capability>` (§6.0).
    /// A `DashMap` so a plugin adapter can be registered at enable time without
    /// rebuilding the shared registry.
    plugin: Arc<dashmap::DashMap<String, Arc<dyn Adapter>>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        AdapterRegistry {
            gemini: Arc::new(crate::adapters::gemini::GeminiAdapter::new()),
            openai: Arc::new(crate::adapters::openai::OpenAiAdapter::new()),
            anthropic: Arc::new(crate::adapters::anthropic::AnthropicAdapter::new()),
            plugin: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub fn for_format(&self, format: WireFormat) -> Arc<dyn Adapter> {
        match format {
            WireFormat::Gemini => self.gemini.clone(),
            WireFormat::Openai => self.openai.clone(),
            WireFormat::Anthropic => self.anthropic.clone(),
            WireFormat::Plugin => Arc::new(UnimplementedAdapter { format: "plugin" }),
        }
    }

    /// Select the adapter for a provider. A provider bound to a plugin adapter
    /// (`wire_plugin`, §6.0) resolves to the plugin adapter when the host
    /// provides it and the plugin is usable; otherwise selection fails closed
    /// with an unimplemented-format error (never a silent native fallback).
    pub fn for_provider(&self, provider: &crate::db::ProviderRow) -> Arc<dyn Adapter> {
        if let Some(r) = provider.wire_plugin_ref() {
            let key = r.to_string_ref();
            if let Some(adapter) = self.plugin.get(&key) {
                return adapter.clone();
            }
            // Registration keys by plugin id (one adapter capability per plugin);
            // accept a bare-id reference too.
            if let Some(adapter) = self.plugin.get(&r.plugin_id) {
                return adapter.clone();
            }
            return Arc::new(UnimplementedAdapter {
                format: Box::leak(
                    format!("plugin adapter '{key}' is unavailable").into_boxed_str(),
                ),
            });
        }
        self.for_format(provider.wire())
    }

    /// Register a plugin-backed adapter under its namespaced reference.
    pub fn register_plugin(&self, reference: impl Into<String>, adapter: Arc<dyn Adapter>) {
        self.plugin.insert(reference.into(), adapter);
    }

    /// Remove every adapter registered for a plugin id: the namespaced
    /// `plugin:<id>/<cap>` keys and the bare id. Called when a plugin is
    /// disabled or removed so a stale adapter cannot keep serving traffic (and
    /// cannot hold a live handle to the plugin manager).
    pub fn unregister_plugin(&self, id: &str) {
        let namespaced = format!("plugin:{id}/");
        self.plugin
            .retain(|key, _| key != id && !key.starts_with(&namespaced));
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A no-op adapter used when a provider's wire format has no built-in
/// implementation yet. Keeps the seam honest: configuration is accepted, calls
/// fail with a clear, format-correct error.
pub struct UnimplementedAdapter {
    pub format: &'static str,
}

#[async_trait]
impl Adapter for UnimplementedAdapter {
    fn wire_format(&self) -> &'static str {
        self.format
    }
    fn build_url(&self, _ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        Err(ProxyError::unsupported(format!(
            "outbound wire format '{}' is not implemented in this build",
            self.format
        )))
    }
    fn apply_auth(
        &self,
        _ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
        Ok(req)
    }
    fn build_body(
        &self,
        _ctx: &UpstreamContext<'_>,
        _req: &InternalRequest,
    ) -> Result<serde_json::Value, UpstreamFailure> {
        Err(UpstreamFailure {
            kind: crate::types::FailureKind::ServerError,
            status: None,
            retry_after_secs: None,
            message: "adapter not implemented".into(),
            quota_reset_at: None,
        })
    }
    fn classify_error(
        &self,
        status: u16,
        _body: &str,
        _headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        UpstreamFailure {
            kind: crate::types::FailureKind::ServerError,
            status: Some(status),
            retry_after_secs: None,
            message: "adapter not implemented".into(),
            quota_reset_at: None,
        }
    }
    fn parse_stream_chunk(&self, _data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(Vec::new())
    }
    fn parse_full_response(
        &self,
        _body: &serde_json::Value,
    ) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod reasoning_discovery_tests {
    use super::*;

    #[test]
    fn normalizes_supported_thinking_efforts_and_derives_openai_map() {
        let metadata = serde_json::json!({
            "supportedThinkingEfforts": ["low", "medium", "high"]
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(
            capability.levels,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        assert_eq!(
            capability.upstream_format,
            "provider_supported_thinking_efforts"
        );
        assert!(!capability.can_disable);
        assert!(thinking_map_for_reasoning(&capability).is_none());

        let map =
            thinking_map_for_reasoning_with_wire(&capability, crate::types::WireFormat::Openai)
                .unwrap();
        assert_eq!(map.level_field.as_deref(), Some("reasoning_effort"));
        assert_eq!(map.levels.get("high"), Some(&serde_json::json!("high")));
    }

    #[test]
    fn nested_reasoning_efforts_do_not_infer_responses_request_dialect() {
        let metadata = serde_json::json!({
            "reasoning": {"supported_efforts": ["minimal", "low", "high"]}
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(
            capability.upstream_format,
            "provider_reasoning_supported_efforts"
        );
        assert_eq!(
            capability.levels,
            vec!["minimal".to_string(), "low".to_string(), "high".to_string()]
        );
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }

    #[test]
    fn recognizes_all_common_metadata_shapes_conservatively() {
        for metadata in [
            serde_json::json!({"supported_reasoning_levels": ["low", "high"]}),
            serde_json::json!({"thinking": {"levels": ["off", "medium", "xhigh"]}}),
            serde_json::json!({"capabilities": {"effort_tiers": ["low", "medium", "high"]}}),
        ] {
            assert!(normalize_reasoning_capability(&metadata).is_some());
        }

        let thinking = normalize_reasoning_capability(
            &serde_json::json!({"thinking": {"levels": ["off", "medium", "xhigh"]}}),
        )
        .unwrap();
        assert!(thinking.can_disable);
        assert!(thinking_map_for_reasoning(&thinking).is_none());
    }

    #[test]
    fn provider_declared_shape_wins_and_unknown_levels_are_not_invented() {
        let metadata = serde_json::json!({
            "supportedThinkingEfforts": ["low", "vendor_ultra"],
            "reasoning": {"supported_efforts": ["high"]}
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(capability.levels, vec!["low".to_string()]);
        assert_eq!(
            capability.upstream_format,
            "provider_supported_thinking_efforts"
        );
    }

    #[test]
    fn preserves_supported_only_plugin_reasoning() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {
                "supported": true
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.mode, None);
        assert!(capability.levels.is_empty());
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }

    #[test]
    fn preserves_toggle_only_plugin_reasoning() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {
                "supported": true,
                "mode": "toggle",
                "can_disable": true
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.mode, Some(ReasoningCapabilityMode::Toggle));
        assert!(capability.levels.is_empty());
        assert!(capability.can_disable);
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }

    #[test]
    fn strict_plugin_v1_rejects_invalid_default_and_unknown_fields() {
        for metadata in [
            serde_json::json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "mode": "level",
                    "levels": ["low"],
                    "default": "max"
                }
            }),
            serde_json::json!({
                "schema_version": 1,
                "unknown": true
            }),
            serde_json::json!({
                "schema_version": 1,
                "reasoning": {
                    "supported": true,
                    "unknown": true
                }
            }),
        ] {
            assert!(normalize_plugin_reasoning_capability_v1(&metadata).is_none());
        }
    }

    #[test]
    fn strict_plugin_v1_exposes_non_reasoning_capabilities() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "reasoning": {"supported": false},
            "tools": {"supported": true},
            "vision": {"input": true},
            "structured_output": {"supported": true}
        });
        let flags = plugin_capability_flags_v1(&metadata).unwrap();
        assert_eq!(flags.reasoning, Some(false));
        assert_eq!(flags.vision, Some(true));
        assert_eq!(flags.tool_calling, Some(true));
        assert_eq!(flags.structured_output, Some(true));
    }

    #[test]
    fn strict_plugin_v1_preserves_reasoning_default() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "transport": {"format": "openai"},
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "max"],
                "default": "max",
                "can_disable": false
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.default.as_deref(), Some("max"));
        assert_eq!(capability.upstream_format, "openai_effort");
    }

    #[test]
    fn strict_plugin_v1_derives_gemini_thinking_level_map() {
        let metadata = serde_json::json!({
            "schema_version": 1,
            "transport": {"format": "gemini"},
            "reasoning": {
                "supported": true,
                "mode": "level",
                "levels": ["low", "medium", "high"],
                "default": "medium",
                "can_disable": false
            }
        });
        let capability = normalize_plugin_reasoning_capability_v1(&metadata).unwrap();
        assert_eq!(capability.upstream_format, "gemini_thinking_level");
        let map =
            thinking_map_for_reasoning_with_wire(&capability, crate::types::WireFormat::Gemini)
                .unwrap();
        assert_eq!(
            map.level_field.as_deref(),
            Some("thinkingConfig.thinkingLevel")
        );
        assert_eq!(map.levels.get("medium"), Some(&serde_json::json!("medium")));
    }

    #[test]
    fn accepts_provider_normalized_adaptive_reasoning_capability() {
        let metadata = serde_json::json!({
            "reasoning_capability": {
                "mode": "adaptive",
                "levels": ["low", "high", "max"],
                "can_disable": false,
                "upstream_format": "anthropic_effort"
            }
        });
        let capability = normalize_reasoning_capability(&metadata).unwrap();
        assert_eq!(capability.mode, Some(ReasoningCapabilityMode::Adaptive));
        assert_eq!(capability.upstream_format, "anthropic_effort");
        assert!(thinking_map_for_reasoning(&capability).is_none());
    }
}
