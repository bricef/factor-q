//! OpenRouter's model catalogue as a pricing source.
//!
//! OpenRouter publishes every model it routes, with prices, at
//! `GET {base_url}/models` — no API key needed. For a model an operator
//! routes through OpenRouter that catalogue is the price actually
//! charged (the LiteLLM table lists the upstream provider's list price,
//! and under a different key: `openrouter/openai/gpt-4o-mini` where
//! factor-q routes `openai/gpt-4o-mini`), so the daemon lays it over
//! the LiteLLM base for exactly those models. Before this source
//! existed every OpenRouter-routed model needed a hand-written
//! `[providers.<name>.pricing]` override to pass the ADR-0004 startup
//! guarantee; the override still wins when present.
//!
//! The catalogue is cached and fallen back on exactly like the LiteLLM
//! JSON ([`super::load_source`]), under its own file, so an OpenRouter
//! outage at boot serves the last copy rather than refusing to start.
//!
//! What the catalogue carries that [`ModelPricing`] cannot hold — a
//! per-request fee (`pricing.request`), per-image and web-search
//! charges — is not modelled here. Those show up in the cost OpenRouter
//! reports on each response instead (`usage.cost`, carried as
//! `reported_cost` on the cost record), which is the figure to
//! reconcile against.

use std::path::Path;

use serde::Deserialize;
use tracing::debug;

use super::{ModelPricing, PricingError, PricingSource, PricingTable, load_source};

/// The host every OpenRouter endpoint lives on. A provider whose
/// `base_url` points here is served by the catalogue.
pub const OPENROUTER_HOST: &str = "openrouter.ai";

/// True when `base_url` is an OpenRouter endpoint — its host is
/// [`OPENROUTER_HOST`], whatever the scheme, port or path. The check is
/// on the host alone so a proxy or a different host that happens to
/// speak the same API is never mistaken for OpenRouter and priced from
/// a catalogue it is not bound by.
pub fn is_openrouter_base_url(base_url: &str) -> bool {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port);
    host.eq_ignore_ascii_case(OPENROUTER_HOST)
}

/// The catalogue endpoint for a provider's `base_url`:
/// `https://openrouter.ai/api/v1` → `https://openrouter.ai/api/v1/models`.
pub fn catalogue_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Fetch the catalogue, cache it at `cache_path`, and parse it — or
/// serve the cached copy, or an empty table. See [`super::load_source`].
pub async fn load(base_url: &str, cache_path: &Path) -> PricingTable {
    let url = catalogue_url(base_url);
    load_source(
        PricingSource {
            name: "OpenRouter",
            url: &url,
            parse: from_openrouter_json,
        },
        cache_path,
    )
    .await
}

/// The catalogue document: `{"data": [<model>, ...]}`.
#[derive(Debug, Deserialize)]
struct Catalogue {
    data: Vec<serde_json::Value>,
}

/// One catalogue entry, the fields read here. Prices are USD per
/// **token**, as decimal strings (`"0.00000015"`); `-1` marks a model
/// whose price is not fixed (the `openrouter/auto` router), which is
/// skipped as unpriced rather than recorded as a negative rate.
#[derive(Debug, Deserialize)]
struct Entry {
    id: String,
    context_length: Option<u32>,
    pricing: Option<Pricing>,
}

#[derive(Debug, Deserialize)]
struct Pricing {
    #[serde(default, deserialize_with = "lenient_price")]
    prompt: Option<f64>,
    #[serde(default, deserialize_with = "lenient_price")]
    completion: Option<f64>,
    #[serde(default, deserialize_with = "lenient_price")]
    input_cache_read: Option<f64>,
    #[serde(default, deserialize_with = "lenient_price")]
    input_cache_write: Option<f64>,
}

/// A price is a decimal string on the wire today and was a bare number
/// before; read both. Anything else, and anything that is not a finite
/// non-negative number, reads as "no price" so one odd entry is skipped
/// rather than failing the document.
fn lenient_price<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let parsed = match value {
        Some(serde_json::Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    Ok(parsed.filter(|p| p.is_finite() && *p >= 0.0))
}

/// Parse the catalogue into a table keyed by OpenRouter's model ids —
/// the ids an operator declares under an OpenRouter provider. An entry
/// with no usable prompt or completion price is skipped, never fatal;
/// its context window is still recorded when present.
pub fn from_openrouter_json(json: &str) -> Result<PricingTable, PricingError> {
    let catalogue: Catalogue =
        serde_json::from_str(json).map_err(|err| PricingError::Parse(err.to_string()))?;
    let mut table = PricingTable::empty();
    let mut skipped = 0usize;
    for value in catalogue.data {
        let entry: Entry = match serde_json::from_value(value) {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if let Some(window) = entry.context_length {
            table.insert_context_window(entry.id.clone(), window);
        }
        let Some(pricing) = entry.pricing else {
            skipped += 1;
            continue;
        };
        let (Some(prompt), Some(completion)) = (pricing.prompt, pricing.completion) else {
            skipped += 1;
            continue;
        };
        table.insert(
            entry.id,
            ModelPricing {
                input_per_million: prompt * 1_000_000.0,
                output_per_million: completion * 1_000_000.0,
                cache_read_per_million: pricing.input_cache_read.map(|c| c * 1_000_000.0),
                cache_write_per_million: pricing.input_cache_write.map(|c| c * 1_000_000.0),
            },
        );
    }
    if skipped > 0 {
        debug!(
            skipped,
            "skipped OpenRouter catalogue entries without a usable price"
        );
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live catalogue's shape as of 2026-09: string prices per token,
    // the auto router's `-1` sentinel, a free model at `"0"`, and an
    // entry with no pricing block at all.
    const CATALOGUE: &str = r#"{
        "data": [
            {
                "id": "openai/gpt-4o-mini",
                "name": "OpenAI: GPT-4o-mini",
                "context_length": 128000,
                "pricing": {
                    "prompt": "0.00000015",
                    "completion": "0.0000006",
                    "request": "0",
                    "image": "0.000217",
                    "input_cache_read": "0.000000075"
                }
            },
            {
                "id": "anthropic/claude-sonnet-4.5",
                "context_length": 1000000,
                "pricing": {
                    "prompt": "0.000003",
                    "completion": "0.000015",
                    "input_cache_read": "0.0000003",
                    "input_cache_write": "0.00000375"
                }
            },
            {
                "id": "openrouter/auto",
                "context_length": 2000000,
                "pricing": { "prompt": "-1", "completion": "-1" }
            },
            {
                "id": "some/free-model:free",
                "context_length": 32768,
                "pricing": { "prompt": "0", "completion": "0" }
            },
            {
                "id": "numeric/prices",
                "pricing": { "prompt": 0.000001, "completion": 0.000002 }
            },
            {
                "id": "no/pricing-block",
                "context_length": 4096
            },
            {
                "id": "garbled/entry",
                "pricing": { "prompt": "abc", "completion": "0.000002" }
            },
            { "not_an_entry": true }
        ]
    }"#;

    #[test]
    fn parses_catalogue_entries_keyed_by_openrouter_id() {
        let table = from_openrouter_json(CATALOGUE).unwrap();
        // Priced: gpt-4o-mini, sonnet, the free model, numeric/prices.
        // Skipped: auto (-1), no/pricing-block, garbled/entry, and
        // the entry that is not an entry.
        assert_eq!(table.len(), 4);

        let mini = table.lookup("openai/gpt-4o-mini").unwrap();
        assert!((mini.input_per_million - 0.15).abs() < 1e-9);
        assert!((mini.output_per_million - 0.60).abs() < 1e-9);
        assert!((mini.cache_read_per_million.unwrap() - 0.075).abs() < 1e-9);
        assert!(mini.cache_write_per_million.is_none());
        assert_eq!(table.context_window("openai/gpt-4o-mini"), Some(128_000));

        let sonnet = table.lookup("anthropic/claude-sonnet-4.5").unwrap();
        assert!((sonnet.input_per_million - 3.0).abs() < 1e-9);
        assert!((sonnet.cache_write_per_million.unwrap() - 3.75).abs() < 1e-9);

        let free = table.lookup("some/free-model:free").unwrap();
        assert_eq!(free.input_per_million, 0.0);

        let numeric = table.lookup("numeric/prices").unwrap();
        assert!((numeric.output_per_million - 2.0).abs() < 1e-9);
    }

    #[test]
    fn an_unfixed_price_is_unpriced_not_negative() {
        let table = from_openrouter_json(CATALOGUE).unwrap();
        assert!(table.lookup("openrouter/auto").is_none());
        // The window is still known — a price and a window are
        // separate facts, as in the LiteLLM parse.
        assert_eq!(table.context_window("openrouter/auto"), Some(2_000_000));
        assert_eq!(table.context_window("no/pricing-block"), Some(4096));
    }

    #[test]
    fn a_document_without_a_data_array_is_a_parse_error() {
        assert!(from_openrouter_json(r#"{"error": "nope"}"#).is_err());
        assert!(from_openrouter_json("not json").is_err());
    }

    #[test]
    fn recognises_openrouter_by_host_alone() {
        assert!(is_openrouter_base_url("https://openrouter.ai/api/v1"));
        assert!(is_openrouter_base_url("https://openrouter.ai/api/v1/"));
        assert!(is_openrouter_base_url("https://OpenRouter.ai:443/api/v1"));
        assert!(is_openrouter_base_url(
            "http://user:pw@openrouter.ai/api/v1"
        ));
        assert!(is_openrouter_base_url("openrouter.ai/api/v1"));

        assert!(!is_openrouter_base_url("https://api.groq.com/openai/v1"));
        assert!(!is_openrouter_base_url(
            "https://proxy.example.com/openrouter.ai/api/v1"
        ));
        assert!(!is_openrouter_base_url("https://notopenrouter.ai/api/v1"));
        assert!(!is_openrouter_base_url("http://127.0.0.1:12345"));
        assert!(!is_openrouter_base_url(""));
    }

    #[test]
    fn catalogue_url_appends_models_once() {
        assert_eq!(
            catalogue_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            catalogue_url("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/models"
        );
    }
}
