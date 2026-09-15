//! `[providers]` — the LLM providers the daemon routes model calls
//! to, and the model **registry** they declare between them: an agent
//! may only name a model some provider lists, and every declared model
//! must resolve to a price or the daemon refuses to start (ADR-0004).

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvidersConfig {
    pub anthropic: Option<AnthropicConfig>,
    /// Additional named providers — `[providers.<name>]` for any name
    /// other than `anthropic`. Each declares an API shape, endpoint,
    /// auth env var, and the model ids it serves, so non-Anthropic
    /// models become available by configuration (ADR-0003).
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, ProviderConfig>,
}

impl ProvidersConfig {
    /// Every model id declared across all providers (anthropic + extra) —
    /// the registry. An agent may only name a model in this set.
    pub fn declared_models(&self) -> impl Iterator<Item = &str> {
        self.anthropic
            .iter()
            .flat_map(|a| a.models.iter())
            .chain(self.extra.values().flat_map(|p| p.models.iter()))
            .map(String::as_str)
    }

    /// Every per-model price override across all providers, as
    /// `(model_id, override)`.
    pub fn pricing_overrides(&self) -> impl Iterator<Item = (&str, &ModelPriceOverride)> {
        self.anthropic
            .iter()
            .flat_map(|a| a.pricing.iter())
            .chain(self.extra.values().flat_map(|p| p.pricing.iter()))
            .map(|(k, v)| (k.as_str(), v))
    }

    /// The providers routed to OpenRouter (by `base_url` host), as
    /// `(name, config)`. Their models are priced from OpenRouter's own
    /// catalogue at startup — see `fq_runtime::pricing::openrouter`.
    pub fn openrouter_providers(&self) -> impl Iterator<Item = (&str, &ProviderConfig)> {
        self.extra
            .iter()
            .filter(|(_, p)| p.is_openrouter())
            .map(|(name, p)| (name.as_str(), p))
    }
}

/// Error listing every model-registry / pricing-coverage violation found
/// at startup. Fail-fast: the daemon refuses to run rather than let an
/// undeclared or unpriced model silently defeat budget enforcement
/// (ADR-0004) by tracking its cost as $0.
#[derive(Debug, thiserror::Error)]
#[error("model registry validation failed:\n  - {}", .problems.join("\n  - "))]
pub struct ModelRegistryError {
    problems: Vec<String>,
}

impl ModelRegistryError {
    /// The individual violation messages.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

/// Validate the model registry and pricing coverage at startup — the
/// ADR-0004 invariant *"a model is available iff it is declared,
/// routable, and priced."*
///
/// 1. every agent's resolved model is **declared** (in some provider's
///    `models = [...]`);
/// 2. the `default_model`, if set, is declared;
/// 3. every declared model resolves to a **price** (the LiteLLM table or
///    a `[providers.<name>.pricing]` override merged into `pricing`).
///
/// All violations are collected so the operator sees the full list at
/// once. `agent_models` is `(agent_id, model)` for readable errors.
pub fn validate_model_registry(
    providers: &ProvidersConfig,
    default_model: Option<&str>,
    agent_models: &[(String, String)],
    pricing: &crate::pricing::PricingTable,
) -> Result<(), ModelRegistryError> {
    use std::collections::BTreeSet;
    let declared: BTreeSet<&str> = providers.declared_models().collect();
    let mut problems = Vec::new();

    if let Some(dm) = default_model
        && !declared.contains(dm)
    {
        problems.push(format!(
            "agents.default_model = \"{dm}\" is not declared under any [providers.<name>] models = [...]"
        ));
    }

    for (id, model) in agent_models {
        if !declared.contains(model.as_str()) {
            problems.push(format!(
                "agent \"{id}\" uses model \"{model}\", not declared under any [providers.<name>] models = [...]"
            ));
        }
    }

    for &model in &declared {
        if pricing.lookup(model).is_none() {
            problems.push(format!(
                "model \"{model}\" is declared but has no pricing — add [providers.<name>.pricing.\"{model}\"] or ensure the LiteLLM table (or, for a model routed through OpenRouter, OpenRouter's catalogue) lists it"
            ));
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(ModelRegistryError { problems })
    }
}

/// API wire shape for a provider — which genai adapter format it speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiShape {
    #[default]
    Anthropic,
    Openai,
    Gemini,
    Ollama,
    OpenaiCompatible,
}

/// A configurable LLM provider: an API shape, an optional endpoint
/// override, an auth env var, and the model ids routed to it.
/// `[providers.<name>]` in `fqd.toml`.
// `deny_unknown_fields` rather than the `serde_ignored` pass `Config`
// uses, because this struct is reached through `ProvidersConfig`'s
// `#[serde(flatten)]`: flattening buffers the table's contents, and an
// unknown key inside a buffer is invisible from outside. It is legal
// here only because this struct itself flattens nothing.
//
// Without it, `api = "openai"` — for `api_shape` — was accepted in
// silence, which is the exact edit an operator is most likely to make.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_shape: ApiShape,
    /// Endpoint override; `None` uses genai's default for the shape.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var holding this provider's API key.
    pub api_key_env: String,
    /// Model ids routed to this provider's endpoint + auth. Also the
    /// provider's slice of the model **registry**: an agent may only
    /// name a model that some provider declares here.
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-model price overrides — `[providers.<name>.pricing."<model>"]`.
    /// Merged over the LiteLLM table so models the table doesn't list
    /// (custom endpoints, OpenRouter-namespaced ids) are still priced,
    /// which the startup pricing guarantee requires (ADR-0004).
    #[serde(default)]
    pub pricing: std::collections::BTreeMap<String, ModelPriceOverride>,
}

impl ProviderConfig {
    /// True when this provider's `base_url` is an OpenRouter endpoint,
    /// which makes OpenRouter's catalogue the price of record for the
    /// models it declares.
    pub fn is_openrouter(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(crate::pricing::openrouter::is_openrouter_base_url)
    }
}

/// A per-model price override in USD per **million** tokens. Merged into
/// the [`crate::pricing::PricingTable`] at startup so an operator can
/// guarantee coverage for a model the LiteLLM table doesn't list.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ModelPriceOverride {
    /// Input (prompt) price, USD per million tokens.
    pub input_per_mtok: f64,
    /// Output (completion) price, USD per million tokens.
    pub output_per_mtok: f64,
    /// Cache-read price; `None` charges cache reads at the input rate.
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
    /// Cache-write price; `None` charges cache writes at the input rate.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

impl ModelPriceOverride {
    /// Convert to a [`crate::pricing::ModelPricing`] entry. The units
    /// already match — the pricing table is keyed in USD per million
    /// tokens — so this is a field copy.
    pub fn to_pricing(&self) -> crate::pricing::ModelPricing {
        crate::pricing::ModelPricing {
            input_per_million: self.input_per_mtok,
            output_per_million: self.output_per_mtok,
            cache_read_per_million: self.cache_read_per_mtok,
            cache_write_per_million: self.cache_write_per_mtok,
            cache_write_1h_per_million: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicConfig {
    #[serde(default = "default_anthropic_api_key_env")]
    pub api_key_env: String,
    /// Optional override for the Anthropic API base URL. When `None`
    /// the genai crate uses Anthropic's public endpoint. Set this to
    /// point at a test mock, an internal proxy, or a future
    /// Bedrock-compatible endpoint.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Anthropic's slice of the model **registry**. Routing for
    /// `claude-*` stays native (genai resolves it), so this list is
    /// purely the declaration that makes those models usable and
    /// subject to the pricing guarantee — list every `claude-*` id the
    /// fleet uses.
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-model price overrides — `[providers.anthropic.pricing."<model>"]`.
    /// Rarely needed (LiteLLM lists Anthropic models), but available for
    /// parity with other providers.
    #[serde(default)]
    pub pricing: std::collections::BTreeMap<String, ModelPriceOverride>,
}

fn default_anthropic_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_anthropic_api_key_env(),
            base_url: None,
            models: Vec::new(),
            pricing: std::collections::BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests;
