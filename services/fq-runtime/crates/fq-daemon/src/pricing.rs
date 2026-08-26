//! The ADR-0004 pricing-coverage guarantee, enforced before anything runs.
//!
//! Split out of `lib.rs` (#189). The daemon is the only thing that drives
//! an agent — D-1 retired the in-process `fq trigger` path — so this is
//! where config overrides are merged over the cached LiteLLM table, and
//! the fail-fast has one place to live rather than two.

use fq_runtime::agent::AgentRegistry;
use fq_runtime::{Config, PricingTable};

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
) -> anyhow::Result<PricingTable> {
    let mut pricing = base;
    let mut overrides = 0usize;
    for (model, ov) in config.providers.pricing_overrides() {
        pricing.insert(model.to_string(), ov.to_pricing());
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
    Ok(pricing)
}
