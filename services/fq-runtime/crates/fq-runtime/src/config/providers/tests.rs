//! Unit tests for [`super`] — the `[providers]` section: a provider of
//! any name parses out of the flattened map, a typo inside one is
//! refused rather than ignored, price overrides reach the table, and
//! the registry names every undeclared or unpriced model at once.

use super::*;
use crate::config::Config;

#[test]
fn extra_providers_parse_as_a_flattened_map() {
    let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[providers.openai]
api_shape = "openai"
api_key_env = "OPENAI_API_KEY"
models = ["gpt-4o-mini"]

[providers.groq]
api_shape = "openai-compatible"
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
models = ["llama-3.1-8b-instant"]
"#;
    let config = Config::from_toml_str(toml).unwrap();
    // anthropic stays on its own named field (back-compat)
    assert!(config.providers.anthropic.is_some());
    // the rest land in the flattened `extra` map, keyed by name
    let extra = &config.providers.extra;
    assert_eq!(
        extra.len(),
        2,
        "keys: {:?}",
        extra.keys().collect::<Vec<_>>()
    );
    let openai = extra.get("openai").expect("openai provider");
    assert_eq!(openai.api_shape, ApiShape::Openai);
    assert_eq!(openai.api_key_env, "OPENAI_API_KEY");
    assert_eq!(openai.models, vec!["gpt-4o-mini".to_string()]);
    let groq = extra.get("groq").expect("groq provider");
    assert_eq!(groq.api_shape, ApiShape::OpenaiCompatible);
    assert_eq!(
        groq.base_url.as_deref(),
        Some("https://api.groq.com/openai/v1")
    );
}

fn priced(input: f64, output: f64) -> crate::pricing::ModelPricing {
    crate::pricing::ModelPricing {
        input_per_million: input,
        output_per_million: output,
        cache_read_per_million: None,
        cache_write_per_million: None,
        cache_write_1h_per_million: None,
    }
}

#[test]
fn validate_model_registry_flags_undeclared_and_unpriced() {
    let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-haiku-4-5"]

[providers.openrouter]
api_shape = "openai-compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
models = ["openai/gpt-4o-mini"]
"#;
    let config = Config::from_toml_str(toml).unwrap();
    // claude priced; openai/gpt-4o-mini deliberately left unpriced.
    let mut pricing = crate::pricing::PricingTable::empty();
    pricing.insert("claude-haiku-4-5", priced(1.0, 2.0));

    let agents = vec![("triage".to_string(), "undeclared-model".to_string())];
    let err = validate_model_registry(
        &config.providers,
        Some("also-undeclared"),
        &agents,
        &pricing,
    )
    .expect_err("expected registry violations");
    let problems = err.problems();

    assert!(
        problems
            .iter()
            .any(|p| p.contains("default_model") && p.contains("also-undeclared")),
        "missing default_model violation: {problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("triage") && p.contains("undeclared-model")),
        "missing agent-model violation: {problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("openai/gpt-4o-mini") && p.contains("no pricing")),
        "missing unpriced violation: {problems:?}"
    );
}

#[test]
fn validate_model_registry_passes_when_declared_and_priced() {
    let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-haiku-4-5"]
"#;
    let config = Config::from_toml_str(toml).unwrap();
    let mut pricing = crate::pricing::PricingTable::empty();
    pricing.insert("claude-haiku-4-5", priced(1.0, 2.0));
    let agents = vec![("triage".to_string(), "claude-haiku-4-5".to_string())];
    validate_model_registry(
        &config.providers,
        Some("claude-haiku-4-5"),
        &agents,
        &pricing,
    )
    .expect("declared + priced should validate");
}

#[test]
fn pricing_override_from_toml_makes_a_model_priced() {
    // Exercises the `[providers.<name>.pricing."<model>"]` shape and
    // the override -> table merge, then validation over it.
    let toml = r#"
[providers.groq]
api_shape = "openai-compatible"
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
models = ["llama-3.1-8b-instant"]
[providers.groq.pricing."llama-3.1-8b-instant"]
input_per_mtok = 0.05
output_per_mtok = 0.08
"#;
    let config = Config::from_toml_str(toml).unwrap();
    let mut pricing = crate::pricing::PricingTable::empty();
    for (model, ov) in config.providers.pricing_overrides() {
        pricing.insert(model.to_string(), ov.to_pricing());
    }
    let entry = pricing
        .lookup("llama-3.1-8b-instant")
        .expect("override merged into the table");
    assert_eq!(entry.input_per_million, 0.05);
    assert_eq!(entry.output_per_million, 0.08);

    let agents = vec![("t".to_string(), "llama-3.1-8b-instant".to_string())];
    validate_model_registry(&config.providers, None, &agents, &pricing)
        .expect("override should satisfy the pricing guarantee");
}

#[test]
fn anthropic_config_parses_base_url_from_toml() {
    let toml = r#"
[providers.anthropic]
base_url = "http://127.0.0.1:12345"
"#;
    let config = Config::from_toml_str(toml).unwrap();
    let anthropic = config.providers.anthropic.unwrap();
    assert_eq!(
        anthropic.base_url.as_deref(),
        Some("http://127.0.0.1:12345")
    );
    // api_key_env still defaults when only base_url is set.
    assert_eq!(anthropic.api_key_env, "ANTHROPIC_API_KEY");
}

#[test]
fn anthropic_config_base_url_defaults_to_none() {
    let toml = r#"
[providers.anthropic]
api_key_env = "SOMETHING"
"#;
    let config = Config::from_toml_str(toml).unwrap();
    assert!(config.providers.anthropic.unwrap().base_url.is_none());
}

#[test]
fn a_provider_of_any_name_is_still_accepted() {
    // `ProvidersConfig` flattens, so the strictness must not cost us
    // the ability to name a provider anything.
    let config = Config::from_toml_str(
        "[providers.openrouter]\napi_shape = \"openai-compatible\"\n\
         api_key_env = \"OPENROUTER_API_KEY\"\n\
         base_url = \"https://openrouter.ai/api/v1\"\n\
         models = [\"z-ai/glm-5.2\"]\n",
    )
    .expect("a named provider is configuration, not a typo");
    assert!(config.providers.extra.contains_key("openrouter"));
}

#[test]
fn a_typo_inside_a_named_provider_is_rejected() {
    // The regression this pairs with: `api` for `api_shape` was
    // accepted in silence, because a flattened map buffers its
    // values and an unknown key inside a buffer is invisible to the
    // `serde_ignored` pass. `ProviderConfig` denies its own unknown
    // fields for exactly this reason.
    //
    // The earlier version of the test above used `api` in its own
    // fixture and passed, which is how the hole stayed open: the
    // test proved a provider could be named, and quietly proved the
    // typo was tolerated too.
    let err = Config::from_toml_str(
        "[providers.openrouter]\napi = \"openai\"\n\
         api_key_env = \"OPENROUTER_API_KEY\"\n\
         models = [\"z-ai/glm-5.2\"]\n",
    )
    .expect_err("`api` is not a field — `api_shape` is");
    let msg = err.to_string();
    assert!(
        msg.contains("api_shape"),
        "the error should name the field meant, got: {msg}"
    );
}
