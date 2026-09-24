use serde::Deserialize;
use serde_json::{json, Value};

const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const MAX_MODELS_DEV_BYTES: usize = 16 * 1024 * 1024;
const BUNDLED_CATALOG_JSON: &str = include_str!("../data/model_capabilities.json");

const MODELS_DEV_PROVIDER_BASES: &[(&str, &str)] = &[
    ("https://api.openai.com/v1", "openai"),
    ("https://api.anthropic.com/v1", "anthropic"),
    ("https://generativelanguage.googleapis.com/v1beta", "google"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    ModelsDev,
    BundledCatalog,
}

impl CatalogSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::ModelsDev => "models.dev",
            Self::BundledCatalog => "bundled_catalog",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CatalogMatch {
    pub source: CatalogSource,
    pub provider_id: String,
    pub host: String,
    pub model_id: String,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities_json: Value,
    pub source_url: Option<String>,
}

impl CatalogMatch {
    pub fn provenance(&self) -> &'static str {
        self.source.label()
    }

    pub fn reference(&self) -> String {
        format!(
            "{}:{}/{}",
            self.source.label(),
            self.provider_id,
            self.model_id
        )
    }
}

#[derive(Debug, Clone)]
pub struct ModelsDevCatalog {
    providers: Value,
}

impl ModelsDevCatalog {
    fn from_slice(bytes: &[u8]) -> Option<Self> {
        let providers: Value = serde_json::from_slice(bytes).ok()?;
        providers.as_object()?;
        Some(Self { providers })
    }

    #[cfg(test)]
    fn from_value(providers: Value) -> Option<Self> {
        providers.as_object()?;
        Some(Self { providers })
    }

    pub async fn fetch(client: &reqwest::Client, base_url: &str) -> Option<Self> {
        if should_skip_external_lookup(base_url) {
            return None;
        }
        let response = client
            .get(MODELS_DEV_API_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = response.bytes().await.ok()?;
        if bytes.len() > MAX_MODELS_DEV_BYTES {
            return None;
        }
        Self::from_slice(&bytes)
    }

    fn provider_id_for_base(&self, base_url: &str) -> Option<String> {
        let normalized_base = normalize_base_url(base_url)?;
        let providers = self.providers.as_object()?;

        if let Some(provider_id) = MODELS_DEV_PROVIDER_BASES
            .iter()
            .find_map(|(base, provider_id)| {
                (normalized_base == *base).then_some((*provider_id).to_string())
            })
            .filter(|provider_id| providers.contains_key(provider_id))
        {
            return Some(provider_id);
        }

        providers.iter().find_map(|(provider_id, provider)| {
            let api = provider.get("api")?.as_str()?;
            (normalize_base_url(api).as_deref() == Some(normalized_base.as_str()))
                .then_some(provider_id.clone())
        })
    }

    fn recognizes_provider(&self, base_url: &str) -> bool {
        self.provider_id_for_base(base_url).is_some()
    }

    pub fn lookup(&self, base_url: &str, model_id: &str) -> Option<CatalogMatch> {
        let providers = self.providers.as_object()?;
        let provider_id = self.provider_id_for_base(base_url)?;
        let provider = providers.get(&provider_id)?;
        let model = provider.get("models")?.as_object()?.get(model_id)?;
        models_dev_match(base_url, &provider_id, model_id, model)
    }
}

fn normalize_base_url(base_url: &str) -> Option<String> {
    let mut url = url::Url::parse(base_url).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let trimmed = url.path().trim_end_matches('/').to_string();
    url.set_path(&trimmed);
    Some(url.to_string().trim_end_matches('/').to_string())
}

fn should_skip_external_lookup(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return true;
    };
    let Some(host) = url.host_str() else {
        return true;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "localhost.localdomain")
        || host.ends_with(".local")
        || host.ends_with(".test")
        || host.ends_with(".invalid")
    {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .ok()
        .is_some_and(|ip| match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local()
            }
        })
}

fn models_dev_match(
    base_url: &str,
    provider_id: &str,
    model_id: &str,
    model: &Value,
) -> Option<CatalogMatch> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();

    let reasoning = model.get("reasoning").and_then(Value::as_bool);
    let mut reasoning_json = reasoning.map(|supported| json!({"supported": supported}));
    if reasoning == Some(true) {
        let mut levels = Vec::new();
        let mut can_disable = false;
        let mut has_toggle = false;
        if let Some(options) = model.get("reasoning_options").and_then(Value::as_array) {
            for option in options {
                match option.get("type").and_then(Value::as_str) {
                    Some("toggle") => {
                        has_toggle = true;
                        can_disable = true;
                    }
                    Some("effort") => {
                        if let Some(values) = option.get("values").and_then(Value::as_array) {
                            for value in values {
                                if value.is_null()
                                    || value.as_str().is_some_and(|value| value == "none")
                                {
                                    can_disable = true;
                                    continue;
                                }
                                let Some(value) = value.as_str() else {
                                    continue;
                                };
                                if matches!(
                                    value,
                                    "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                                ) && !levels.iter().any(|level| level == value)
                                {
                                    levels.push(value.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        reasoning_json = Some(if !levels.is_empty() {
            json!({
                "supported": true,
                "mode": "level",
                "levels": levels,
                "can_disable": can_disable,
            })
        } else if has_toggle {
            json!({
                "supported": true,
                "mode": "toggle",
                "can_disable": true,
            })
        } else {
            json!({
                "supported": true,
                "can_disable": can_disable,
            })
        });
    }

    let mut capabilities = serde_json::Map::new();
    capabilities.insert("schema_version".to_string(), json!(1));
    if let Some(reasoning) = reasoning_json {
        capabilities.insert("reasoning".to_string(), reasoning);
    }
    if let Some(tool_call) = model.get("tool_call").and_then(Value::as_bool) {
        capabilities.insert("tools".to_string(), json!({"supported": tool_call}));
    }
    if let Some(structured_output) = model.get("structured_output").and_then(Value::as_bool) {
        capabilities.insert(
            "structured_output".to_string(),
            json!({"supported": structured_output}),
        );
    }
    if let Some(inputs) = model.pointer("/modalities/input").and_then(Value::as_array) {
        capabilities.insert(
            "vision".to_string(),
            json!({
                "input": inputs
                    .iter()
                    .any(|input| input.as_str().is_some_and(|input| input == "image"))
            }),
        );
    }

    Some(CatalogMatch {
        source: CatalogSource::ModelsDev,
        provider_id: provider_id.to_string(),
        host,
        model_id: model_id.to_string(),
        context_window: model.pointer("/limit/context").and_then(Value::as_i64),
        max_output_tokens: model.pointer("/limit/output").and_then(Value::as_i64),
        capabilities_json: Value::Object(capabilities),
        source_url: Some(MODELS_DEV_API_URL.to_string()),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledCatalog {
    schema_version: u32,
    providers: Vec<BundledProviderCatalog>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledProviderCatalog {
    base_urls: Vec<String>,
    models: Vec<BundledCatalogModel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledCatalogModel {
    id: String,
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities_json: Value,
    source_url: Option<String>,
}

fn lookup_bundled_from_str(
    catalog_json: &str,
    base_url: &str,
    model_id: &str,
) -> Option<CatalogMatch> {
    let url = url::Url::parse(base_url).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let normalized_base = normalize_base_url(base_url)?;
    let catalog: BundledCatalog = serde_json::from_str(catalog_json).ok()?;
    if catalog.schema_version != 1 {
        return None;
    }

    for provider in catalog.providers {
        if !provider.base_urls.iter().any(|candidate| {
            normalize_base_url(candidate).as_deref() == Some(normalized_base.as_str())
        }) {
            continue;
        }
        let Some(model) = provider
            .models
            .into_iter()
            .find(|model| model.id == model_id)
        else {
            continue;
        };
        return Some(CatalogMatch {
            source: CatalogSource::BundledCatalog,
            provider_id: host.clone(),
            host,
            model_id: model.id,
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            capabilities_json: model.capabilities_json,
            source_url: model.source_url,
        });
    }
    None
}

pub fn resolve(
    base_url: &str,
    model_id: &str,
    models_dev: Option<&ModelsDevCatalog>,
) -> Option<CatalogMatch> {
    resolve_with_bundled(base_url, model_id, models_dev, BUNDLED_CATALOG_JSON)
}

fn resolve_with_bundled(
    base_url: &str,
    model_id: &str,
    models_dev: Option<&ModelsDevCatalog>,
    bundled_json: &str,
) -> Option<CatalogMatch> {
    if let Some(catalog) = models_dev {
        // A recognized models.dev provider is authoritative for catalog
        // membership. Missing models stay unknown; bundled data only fills
        // providers that models.dev does not know about at all.
        if catalog.recognizes_provider(base_url) {
            return catalog.lookup(base_url, model_id);
        }
    }
    lookup_bundled_from_str(bundled_json, base_url, model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models_dev_fixture() -> ModelsDevCatalog {
        ModelsDevCatalog::from_value(json!({
            "openai": {
                "id": "openai",
                "name": "OpenAI",
                "models": {}
            },
            "anthropic": {
                "id": "anthropic",
                "name": "Anthropic",
                "models": {}
            },
            "google": {
                "id": "google",
                "name": "Google",
                "models": {
                    "gemini-3.8-flash": {
                        "id": "gemini-3.8-flash",
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["low", "medium", "high"]}
                        ],
                        "tool_call": true,
                        "structured_output": true,
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "limit": {"context": 1048576, "output": 65536}
                    }
                }
            },
            "openrouter": {
                "id": "openrouter",
                "api": "https://openrouter.ai/api/v1",
                "models": {
                    "shared-model": {
                        "id": "shared-model",
                        "reasoning": false,
                        "tool_call": false,
                        "structured_output": false,
                        "modalities": {"input": ["text"], "output": ["text"]},
                        "limit": {"context": 32000, "output": 4096}
                    }
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn resolves_first_party_and_api_declaring_provider_identity() {
        let catalog = models_dev_fixture();
        assert_eq!(
            catalog
                .provider_id_for_base("https://api.openai.com/v1")
                .as_deref(),
            Some("openai")
        );
        assert_eq!(
            catalog
                .provider_id_for_base("https://api.anthropic.com/v1/")
                .as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            catalog
                .provider_id_for_base("https://generativelanguage.googleapis.com/v1beta")
                .as_deref(),
            Some("google")
        );
        assert_eq!(
            catalog
                .provider_id_for_base("https://openrouter.ai/api/v1")
                .as_deref(),
            Some("openrouter")
        );
    }

    #[test]
    fn matches_exact_models_dev_provider_and_model() {
        let catalog = models_dev_fixture();
        let model = catalog
            .lookup(
                "https://generativelanguage.googleapis.com/v1beta/",
                "gemini-3.8-flash",
            )
            .unwrap();
        assert_eq!(model.provenance(), "models.dev");
        assert_eq!(model.provider_id, "google");
        assert_eq!(model.context_window, Some(1_048_576));
        assert_eq!(model.max_output_tokens, Some(65_536));
        assert_eq!(
            model.capabilities_json["reasoning"]["levels"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(model.capabilities_json["vision"]["input"], true);
    }

    #[test]
    fn models_dev_api_base_url_matching_is_exact() {
        let catalog = models_dev_fixture();
        assert!(catalog
            .lookup("https://openrouter.ai/api/v1", "shared-model")
            .is_some());
        assert!(catalog
            .lookup("https://other.example/v1", "shared-model")
            .is_none());
        assert!(catalog
            .lookup("https://openrouter.ai/api/v1", "missing-model")
            .is_none());
    }

    #[test]
    fn models_dev_wins_over_bundled_data() {
        let catalog = models_dev_fixture();
        let bundled = r#"{
          "schema_version": 1,
          "providers": [{
            "base_urls": ["https://generativelanguage.googleapis.com/v1beta"],
            "models": [{
              "id": "gemini-3.8-flash",
              "context_window": 1,
              "max_output_tokens": 2,
              "capabilities_json": {
                "schema_version": 1,
                "reasoning": {"supported": false}
              },
              "source_url": "https://example.invalid/bundled"
            }]
          }]
        }"#;
        let model = resolve_with_bundled(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-3.8-flash",
            Some(&catalog),
            bundled,
        )
        .unwrap();
        assert_eq!(model.provenance(), "models.dev");
        assert_eq!(model.context_window, Some(1_048_576));
        assert_eq!(model.capabilities_json["reasoning"]["supported"], true);
    }

    #[test]
    fn recognized_models_dev_provider_with_missing_model_does_not_use_bundled() {
        let catalog = models_dev_fixture();
        let bundled = r#"{
          "schema_version": 1,
          "providers": [{
            "base_urls": ["https://generativelanguage.googleapis.com/v1beta"],
            "models": [{
              "id": "bundled-only-model",
              "context_window": 999,
              "max_output_tokens": 111,
              "capabilities_json": {
                "schema_version": 1,
                "reasoning": {"supported": true}
              },
              "source_url": "https://example.invalid/bundled"
            }]
          }]
        }"#;

        assert!(resolve_with_bundled(
            "https://generativelanguage.googleapis.com/v1beta",
            "bundled-only-model",
            Some(&catalog),
            bundled,
        )
        .is_none());
    }

    #[test]
    fn bundled_data_is_used_when_models_dev_provider_is_absent() {
        let catalog = models_dev_fixture();
        let bundled = r#"{
          "schema_version": 1,
          "providers": [{
            "base_urls": ["https://api.b.ai/v1"],
            "models": [{
              "id": "DeepSeek-V4.1-Flash",
              "context_window": 1000000,
              "max_output_tokens": 384000,
              "capabilities_json": {
                "schema_version": 1,
                "reasoning": {
                  "supported": true,
                  "mode": "level",
                  "levels": ["low", "high", "max"],
                  "can_disable": false
                }
              },
              "source_url": "https://docs.b.ai/llmservice/models/deepseek-v4-1-flash/"
            }]
          }]
        }"#;
        let model = resolve_with_bundled(
            "https://api.b.ai/v1",
            "DeepSeek-V4.1-Flash",
            Some(&catalog),
            bundled,
        )
        .unwrap();
        assert_eq!(model.provenance(), "bundled_catalog");
    }

    #[test]
    fn bundled_catalog_does_not_duplicate_models_dev_google_entry() {
        assert!(resolve(
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-3.8-flash",
            None,
        )
        .is_none());
    }

    #[test]
    fn bundled_provider_matching_is_exact() {
        assert!(resolve("https://api.b.ai/other", "DeepSeek-V4.1-Flash", None,).is_none());
    }

    #[test]
    fn models_dev_unavailability_falls_back_to_bundled_data() {
        let model = resolve("https://api.b.ai/v1", "DeepSeek-V4.1-Flash", None).unwrap();
        assert_eq!(model.provenance(), "bundled_catalog");
        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(384_000));
    }
}
