//! The ADR-0004 pricing-coverage guarantee, enforced before anything runs.
//!
//! Split out of `lib.rs` (#189). The daemon is the only thing that drives
//! an agent — D-1 retired the in-process `fq trigger` path — so this is
//! where the pricing sources are layered, config overrides are merged
//! over them, and the fail-fast has one place to live rather than two.
//!
//! The layers, lowest first: the LiteLLM table; OpenRouter's catalogue
//! for the models declared under an OpenRouter provider (keyed by the
//! id factor-q routes, which LiteLLM lists under `openrouter/…` if at
//! all); the operator's `[providers.<name>.pricing]` overrides.

use std::collections::BTreeSet;
use std::path::Path;

use fq_runtime::agent::AgentRegistry;
use fq_runtime::events::{OperatorSignalPayload, PricingProvenance};
use fq_runtime::pricing::{live, openrouter};
use fq_runtime::{Config, PricingOverlay, PricingRefresh, PricingTable, ServedPricing};

/// The LiteLLM snapshot's file name under the cache directory.
const LITELLM_CACHE_FILE: &str = "pricing.json";
/// The OpenRouter catalogue's file name under the cache directory.
const OPENROUTER_CACHE_FILE: &str = "openrouter-pricing.json";

/// What the pricing load wants said about it once the bus is up: how
/// many models it priced, which table it accepted, and what an operator
/// should look at.
///
/// One value rather than three fields on `Assembled`: they are one
/// fact — this run's price list — and `run_hosted` threads them to one
/// place, the startup announcement.
pub(crate) struct PricingStartup {
    /// Entries in the merged table, after overrides.
    pub(crate) entries: u32,
    /// The accepted LiteLLM table this run loaded (#735). Cited by every
    /// cost record the run writes.
    pub(crate) provenance: Option<PricingProvenance>,
    /// Refusals, a failed fetch, a stale table — one signal each.
    pub(crate) signals: Vec<OperatorSignalPayload>,
}

/// Load the pricing sources the config calls for: always LiteLLM, plus
/// OpenRouter's catalogue when some provider is routed there. Each is
/// fetched, cached and fallen back on independently; the result is the
/// base [`build_validated_pricing`] merges overrides into.
///
/// The LiteLLM table goes through acceptance (#735) and comes back with
/// its provenance and whatever the acceptance step wants an operator to
/// know; the returned signals are published once the bus is announced.
/// An unreadable `[pricing]` section is an error rather than a fallback:
/// an operator who asked for a pin and got the live document has the
/// opposite of what they configured.
pub(crate) async fn load_pricing_sources(config: &Config) -> anyhow::Result<LoadedPricing> {
    let cache_dir = &config.cache.directory;
    let settings = config.pricing.load_settings()?;
    let load = live::load_accepted(settings, &cache_dir.join(LITELLM_CACHE_FILE)).await;
    let signals = load.signals();
    let mut overlay = PricingOverlay::new();
    let mut pricing = load.table;
    // Group by endpoint so one catalogue serves every provider on it;
    // in practice there is one OpenRouter provider, but two would be
    // one fetch, not two.
    let mut by_endpoint: Vec<(&str, Vec<&str>)> = Vec::new();
    for (_, provider) in config.providers.openrouter_providers() {
        let base_url = provider.base_url.as_deref().unwrap_or_default();
        let models = provider.models.iter().map(String::as_str);
        match by_endpoint.iter_mut().find(|(url, _)| *url == base_url) {
            Some((_, list)) => list.extend(models),
            None => by_endpoint.push((base_url, models.collect())),
        }
    }
    for (base_url, models) in by_endpoint {
        let catalogue = openrouter::load(base_url, &cache_dir.join(OPENROUTER_CACHE_FILE)).await;
        let coverage = price_openrouter_models(&mut pricing, &catalogue, models);
        coverage.record(&pricing, &mut overlay);
        coverage.report();
    }
    Ok(LoadedPricing {
        table: pricing,
        signals,
        overlay,
    })
}

/// What the startup load produced, before the operator's overrides and
/// the coverage guarantee.
pub(crate) struct LoadedPricing {
    /// The accepted LiteLLM table with the OpenRouter catalogue layered
    /// over it.
    pub(crate) table: PricingTable,
    /// What the load wants an operator told, once the bus is announced.
    pub(crate) signals: Vec<OperatorSignalPayload>,
    /// Which of those prices are *not* the LiteLLM table's, so a later
    /// refresh does not overwrite them with it (#344).
    pub(crate) overlay: PricingOverlay,
}

/// How the models routed through OpenRouter came to be priced.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct OpenRouterCoverage {
    /// Priced from OpenRouter's catalogue — the price of record.
    pub(crate) from_catalogue: BTreeSet<String>,
    /// Not in the catalogue (or the catalogue was unavailable), priced
    /// from LiteLLM's `openrouter/<id>` entry instead.
    pub(crate) from_litellm: BTreeSet<String>,
    /// Priced by neither. An override may still cover these; otherwise
    /// the startup guarantee names them and refuses to run.
    pub(crate) unpriced: BTreeSet<String>,
}

impl OpenRouterCoverage {
    /// Record every routed id as a price the LiteLLM table does not get
    /// to set (#344).
    ///
    /// Both halves, not just `from_catalogue`: a model priced
    /// `from_litellm` was priced from a *differently spelled* key —
    /// `openrouter/openai/gpt-4o-mini` for the id `openai/gpt-4o-mini` —
    /// so a refresh of the LiteLLM table would leave the routed id
    /// exactly where it was anyway. Recording it says so explicitly,
    /// which is what keeps the refresh's "no longer listed upstream"
    /// count from naming every routed model on every run.
    fn record(&self, table: &PricingTable, overlay: &mut PricingOverlay) {
        for model in self.from_catalogue.iter().chain(&self.from_litellm) {
            if let Some(pricing) = table.lookup(model) {
                overlay.set(model, *pricing);
            }
        }
    }

    fn report(&self) {
        if !self.from_catalogue.is_empty() || !self.from_litellm.is_empty() {
            println!(
                "OpenRouter models priced: {} from the catalogue, {} from LiteLLM",
                self.from_catalogue.len(),
                self.from_litellm.len()
            );
        }
        for model in &self.unpriced {
            eprintln!(
                "warning: OpenRouter model \"{model}\" is in neither the catalogue nor LiteLLM; \
                 it needs a [providers.<name>.pricing] override"
            );
        }
    }
}

/// Price each OpenRouter-routed model in `models` under its routed id:
/// from `catalogue` when it lists the model, else from `pricing`'s own
/// LiteLLM `openrouter/<id>` entry, else not at all. A catalogue price
/// replaces whatever the base held for that id — the model is routed
/// through OpenRouter, so OpenRouter's figure is the one charged.
pub(crate) fn price_openrouter_models<'a>(
    pricing: &mut PricingTable,
    catalogue: &PricingTable,
    models: impl IntoIterator<Item = &'a str>,
) -> OpenRouterCoverage {
    let mut coverage = OpenRouterCoverage::default();
    for model in models {
        if pricing.adopt(catalogue, model) {
            coverage.from_catalogue.insert(model.to_string());
        } else if pricing.adopt_prefixed("openrouter", model) || pricing.lookup(model).is_some() {
            coverage.from_litellm.insert(model.to_string());
        } else {
            coverage.unpriced.insert(model.to_string());
        }
    }
    coverage
}

/// Where the LiteLLM snapshot lives for a cache directory — the path
/// the boot banner prints.
pub(crate) fn litellm_cache_path(cache_dir: &Path) -> std::path::PathBuf {
    cache_dir.join(LITELLM_CACHE_FILE)
}

/// Merge `[providers.<name>.pricing]` overrides over the loaded LiteLLM
/// table, then enforce the ADR-0004 coverage guarantee: every declared
/// model is priced, and every agent model + `agents.default_model` is
/// declared. Fail-fast — the daemon refuses to run rather than let an
/// undeclared or unpriced model silently track its cost as $0 and defeat
/// budget enforcement. Returns the merged table on success.
pub(crate) fn build_validated_pricing(
    config: &Config,
    registry: &AgentRegistry,
    base: PricingTable,
    mut overlay: PricingOverlay,
) -> anyhow::Result<(PricingTable, PricingOverlay)> {
    let mut pricing = base;
    let mut overrides = 0usize;
    for (model, ov) in config.providers.pricing_overrides() {
        pricing.insert(model.to_string(), ov.to_pricing());
        // An override is the operator's answer to a price the source
        // gets wrong or does not carry, so it outranks every table a
        // refresh will ever accept (#344).
        overlay.set(model, ov.to_pricing());
        overrides += 1;
    }
    if overrides > 0 {
        println!("Applied {overrides} model pricing override(s) from config");
    }
    let mut agent_models: Vec<(String, String)> = registry
        .iter()
        .map(|l| {
            (
                l.agent.id().as_str().to_string(),
                l.agent.model().to_string(),
            )
        })
        .collect();
    // The summariser's model (#216) is held to the same guarantee as
    // agent models: routed by a provider and priced, or refuse to
    // start — its spend is cost-accounted like everyone else's.
    if let Some(model) = &config.summary.model {
        agent_models.push(("summary".to_string(), model.clone()));
    }
    fq_runtime::config::validate_model_registry(
        &config.providers,
        config.agents.default_model.as_deref(),
        &agent_models,
        &pricing,
    )?;
    Ok((pricing, overlay))
}

/// The daemon's price list as the hosted tasks take it: the table they
/// read, and the refresh that swaps it.
///
/// One value rather than two `Assembled` fields, because they are one
/// fact — this daemon's prices — and the refresh already holds the
/// handle the readers read.
pub(crate) struct DaemonPricing {
    /// The handle every cost path reads through.
    pub(crate) served: ServedPricing,
    /// The scheduled refresh, handed to the maintenance consumer.
    pub(crate) refresh: PricingRefresh,
}

impl DaemonPricing {
    /// Serve `table`, and prepare the refresh that will replace it:
    /// the same `[pricing]` settings and the same cache file the
    /// startup load used, so a refresh judges each document against the
    /// one this daemon booted on.
    pub(crate) fn new(
        config: &Config,
        table: PricingTable,
        overlay: PricingOverlay,
    ) -> anyhow::Result<Self> {
        let served = ServedPricing::new(table);
        let refresh = PricingRefresh::new(
            config.pricing.load_settings()?,
            litellm_cache_path(&config.cache.directory),
            overlay,
            served.clone(),
        );
        Ok(Self { served, refresh })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fq_runtime::ModelPricing;

    fn priced(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cache_read_per_million: None,
            cache_write_per_million: None,
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_catalogue_prices_routed_ids_and_litellm_fills_the_gaps() {
        // LiteLLM: the gateway-prefixed key for gpt-4o-mini, a bare key
        // that must not be mistaken for the routed id, and nothing for
        // the third model.
        let mut pricing = PricingTable::empty();
        pricing.insert("openrouter/openai/gpt-4o-mini", priced(0.15, 0.60));
        pricing.insert("openrouter/deepseek/deepseek-chat", priced(0.5, 1.0));
        pricing.insert("gpt-4o-mini", priced(0.15, 0.60));

        // The catalogue lists gpt-4o-mini (with OpenRouter's own figure)
        // but not deepseek, and not the third model.
        let mut catalogue = PricingTable::empty();
        catalogue.insert("openai/gpt-4o-mini", priced(0.16, 0.64));
        catalogue.insert_context_window("openai/gpt-4o-mini", 128_000);

        let coverage = price_openrouter_models(
            &mut pricing,
            &catalogue,
            [
                "openai/gpt-4o-mini",
                "deepseek/deepseek-chat",
                "nobody/lists-this",
            ],
        );

        assert_eq!(
            coverage,
            OpenRouterCoverage {
                from_catalogue: set(&["openai/gpt-4o-mini"]),
                from_litellm: set(&["deepseek/deepseek-chat"]),
                unpriced: set(&["nobody/lists-this"]),
            }
        );
        let mini = pricing.lookup("openai/gpt-4o-mini").unwrap();
        assert!((mini.input_per_million - 0.16).abs() < 1e-9);
        assert_eq!(pricing.context_window("openai/gpt-4o-mini"), Some(128_000));
        assert!(
            (pricing
                .lookup("deepseek/deepseek-chat")
                .unwrap()
                .output_per_million
                - 1.0)
                .abs()
                < 1e-9
        );
        assert!(pricing.lookup("nobody/lists-this").is_none());
    }

    #[test]
    fn the_catalogue_replaces_a_base_price_for_a_routed_model() {
        // A base entry under the routed id (say, an operator's earlier
        // hand-priced snapshot) yields to the catalogue: the model is
        // routed through OpenRouter, so OpenRouter's figure is charged.
        let mut pricing = PricingTable::empty();
        pricing.insert("anthropic/claude-haiku-4.5", priced(9.0, 9.0));
        let mut catalogue = PricingTable::empty();
        catalogue.insert("anthropic/claude-haiku-4.5", priced(1.0, 5.0));

        let coverage =
            price_openrouter_models(&mut pricing, &catalogue, ["anthropic/claude-haiku-4.5"]);
        assert_eq!(coverage.from_catalogue.len(), 1);
        assert!(
            (pricing
                .lookup("anthropic/claude-haiku-4.5")
                .unwrap()
                .input_per_million
                - 1.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn an_empty_catalogue_leaves_litellm_to_price_what_it_can() {
        // The catalogue was unreachable and never cached: the models it
        // would have priced fall through to LiteLLM, and a model
        // LiteLLM already lists under the routed id stays priced.
        let mut pricing = PricingTable::empty();
        pricing.insert("openrouter/openai/gpt-4o-mini", priced(0.15, 0.60));
        pricing.insert("google/gemini-2.5-flash", priced(0.3, 2.5));

        let coverage = price_openrouter_models(
            &mut pricing,
            &PricingTable::empty(),
            ["openai/gpt-4o-mini", "google/gemini-2.5-flash", "x/unknown"],
        );
        assert_eq!(
            coverage.from_litellm,
            set(&["openai/gpt-4o-mini", "google/gemini-2.5-flash"])
        );
        assert_eq!(coverage.unpriced, set(&["x/unknown"]));
        assert!(coverage.from_catalogue.is_empty());
    }
}
