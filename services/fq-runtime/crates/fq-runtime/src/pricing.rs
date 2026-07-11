//! Model pricing table sourced from the LiteLLM project.
//!
//! Adopts the [LiteLLM pricing JSON](https://github.com/BerriAI/litellm)
//! as the source of truth for model input/output prices and cache
//! pricing. LiteLLM maintains the file as providers change prices, which
//! is more sustainable than hand-coding ~15 entries ourselves.
//!
//! Loading strategy (see [`PricingTable::load`]):
//! 1. Fetch the JSON from GitHub.
//! 2. On success, write it to the cache path and parse the fresh copy.
//! 3. On fetch failure, log a warning and load the last cached copy.
//! 4. On cache miss too, log another warning and return an empty table
//!    (costs will be reported as $0 with a warning per unknown model).
//!
//! The runtime never blocks on pricing. Agents keep running even if we
//! fall back to a stale cache or an empty table.
//!
//! Note: this is a startup fetch. Once factor-q is a continuously
//! running service (per VISION.md), pricing will need periodic refresh
//! through the future internal job scheduler — see the phase 1 plan's
//! deferred work section.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::events::TokenUsage;

/// URL of the LiteLLM pricing JSON, main branch.
pub const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Per-model input and output prices in USD per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: Option<f64>,
    pub cache_write_per_million: Option<f64>,
}

impl ModelPricing {
    /// Calculate the total cost in USD for the given usage.
    ///
    /// Returns `(input_cost, output_cost, total_cost)` where
    /// `input_cost` covers the whole prompt: the uncached portion at
    /// the base input rate, cache reads at the (discounted) read rate,
    /// and cache writes at the (premium) write rate. `usage.input_tokens`
    /// is the *total* prompt size with the cache counts as subsets of
    /// it — see [`TokenUsage`] — so the uncached portion is derived by
    /// subtraction. Models without cache rates in the pricing table
    /// charge cache tokens at the base input rate (conservative; a
    /// provider that reports cache hits without published discounts is
    /// never under-billed).
    pub fn calculate(&self, usage: &TokenUsage) -> (f64, f64, f64) {
        let read = usage.cache_read_tokens;
        let write = usage.cache_write_tokens;
        let uncached = usage
            .input_tokens
            .saturating_sub(read.saturating_add(write));
        let read_rate = self
            .cache_read_per_million
            .unwrap_or(self.input_per_million);
        let write_rate = self
            .cache_write_per_million
            .unwrap_or(self.input_per_million);
        let input_cost = ((uncached as f64) * self.input_per_million
            + (read as f64) * read_rate
            + (write as f64) * write_rate)
            / 1_000_000.0;
        let output_cost = (usage.output_tokens as f64) * self.output_per_million / 1_000_000.0;
        (input_cost, output_cost, input_cost + output_cost)
    }
}

/// Table of model pricing, keyed by model identifier.
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: HashMap<String, ModelPricing>,
    /// Model context-window sizes (`max_input_tokens` from the LiteLLM
    /// JSON), keyed by model identifier. Kept alongside prices — the
    /// same source file carries both — but in a separate map so adding
    /// it did not disturb the many `ModelPricing` construction sites.
    /// Populated only for models the source lists a window for; absent
    /// otherwise (looked up as `None`). See [`PricingTable::context_window`].
    context_windows: HashMap<String, u32>,
}

impl PricingTable {
    /// Create an empty pricing table.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct a table directly from a map — used in tests.
    pub fn from_map(entries: HashMap<String, ModelPricing>) -> Self {
        Self {
            entries,
            context_windows: HashMap::new(),
        }
    }

    /// Number of models in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up pricing for a given model identifier.
    pub fn lookup(&self, model: &str) -> Option<&ModelPricing> {
        self.entries.get(model)
    }

    /// Look up the context-window size (max input tokens) for a model,
    /// sourced from the LiteLLM JSON `max_input_tokens` field. `None`
    /// when the source lists no window for the model — callers must
    /// treat the window as unknown rather than assume a default.
    pub fn context_window(&self, model: &str) -> Option<u32> {
        self.context_windows.get(model).copied()
    }

    /// Insert or replace a model's pricing. Used to merge config
    /// `[providers.<name>.pricing]` overrides over the LiteLLM table at
    /// startup, so operators can guarantee coverage for models the table
    /// doesn't list (ADR-0004 pricing guarantee).
    pub fn insert(&mut self, model: impl Into<String>, pricing: ModelPricing) {
        self.entries.insert(model.into(), pricing);
    }

    /// Parse a LiteLLM-format pricing JSON string into a table.
    ///
    /// Unknown or malformed entries are skipped rather than failing the
    /// whole parse — one model with a missing price shouldn't prevent
    /// loading the rest.
    pub fn from_litellm_json(json: &str) -> Result<Self, PricingError> {
        let raw: HashMap<String, LiteLlmEntry> =
            serde_json::from_str(json).map_err(|err| PricingError::Parse(err.to_string()))?;

        let mut entries = HashMap::with_capacity(raw.len());
        let mut context_windows = HashMap::new();
        for (model, entry) in raw {
            // LiteLLM's file includes a "sample_spec" entry used as a
            // schema template; it has no real pricing.
            if model == "sample_spec" {
                continue;
            }
            // The context window is recorded whenever the source lists
            // it, independent of whether the entry also carries prices —
            // a model can have a known window but be skipped for pricing.
            if let Some(window) = entry.max_input_tokens {
                context_windows.insert(model.clone(), window);
            }
            let Some(input) = entry.input_cost_per_token else {
                continue;
            };
            let Some(output) = entry.output_cost_per_token else {
                continue;
            };
            entries.insert(
                model,
                ModelPricing {
                    input_per_million: input * 1_000_000.0,
                    output_per_million: output * 1_000_000.0,
                    cache_read_per_million: entry
                        .cache_read_input_token_cost
                        .map(|c| c * 1_000_000.0),
                    cache_write_per_million: entry
                        .cache_creation_input_token_cost
                        .map(|c| c * 1_000_000.0),
                },
            );
        }
        Ok(Self {
            entries,
            context_windows,
        })
    }

    /// Load the pricing table: fetch from LiteLLM, cache to disk, fall
    /// back to the cached copy on failure, and return an empty table if
    /// neither source is available.
    pub async fn load(cache_path: &Path) -> Self {
        match Self::fetch(LITELLM_PRICING_URL).await {
            Ok(json) => {
                debug!(bytes = json.len(), "fetched LiteLLM pricing JSON");
                if let Err(err) = write_cache(cache_path, &json) {
                    warn!(error = %err, "failed to write pricing cache");
                }
                match Self::from_litellm_json(&json) {
                    Ok(table) => {
                        info!(entries = table.len(), "loaded pricing from LiteLLM");
                        table
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to parse fetched pricing JSON");
                        Self::load_from_cache_or_empty(cache_path)
                    }
                }
            }
            Err(err) => {
                warn!(error = %err, "failed to fetch LiteLLM pricing; using cached copy");
                Self::load_from_cache_or_empty(cache_path)
            }
        }
    }

    fn load_from_cache_or_empty(cache_path: &Path) -> Self {
        match fs::read_to_string(cache_path) {
            Ok(json) => match Self::from_litellm_json(&json) {
                Ok(table) => {
                    // Serving from disk means the fresh fetch didn't
                    // land — surface it loudly with the cache age so
                    // reliance on possibly-stale prices is visible. The
                    // startup pricing guarantee still ensures declared
                    // models are *priced*; this flags that they may be
                    // *out of date* (runtime refresh is future work —
                    // docs/plans/backlog.md, "Scheduled refresh of
                    // pricing data").
                    warn!(
                        entries = table.len(),
                        path = %cache_path.display(),
                        cache_age = %cache_age(cache_path),
                        "using cached LiteLLM pricing (fresh fetch unavailable); prices may be stale"
                    );
                    table
                }
                Err(err) => {
                    warn!(error = %err, path = %cache_path.display(), "cached pricing is corrupt; using empty table");
                    Self::empty()
                }
            },
            Err(_) => {
                warn!(
                    path = %cache_path.display(),
                    "no cached pricing available; costs will be reported as $0"
                );
                Self::empty()
            }
        }
    }

    async fn fetch(url: &str) -> Result<String, PricingError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("factor-q/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| PricingError::Http(err.to_string()))?;

        let response = client
            .get(url)
            .send()
            .await
            .map_err(|err| PricingError::Http(err.to_string()))?;

        if !response.status().is_success() {
            return Err(PricingError::Http(format!(
                "unexpected status: {}",
                response.status()
            )));
        }

        response
            .text()
            .await
            .map_err(|err| PricingError::Http(err.to_string()))
    }
}

fn write_cache(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

/// Best-effort human-readable age of the pricing cache file, for the
/// cold-cache warning. Returns `"unknown age"` if the mtime can't be read
/// and `"just now"` if the clock appears to have moved backwards.
fn cache_age(path: &Path) -> String {
    use std::time::SystemTime;
    let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) else {
        return "unknown age".to_string();
    };
    match SystemTime::now().duration_since(modified) {
        Ok(elapsed) => {
            let secs = elapsed.as_secs();
            if secs < 3_600 {
                format!("{}m", secs / 60)
            } else if secs < 86_400 {
                format!("{}h", secs / 3_600)
            } else {
                format!("{}d", secs / 86_400)
            }
        }
        Err(_) => "just now".to_string(),
    }
}

/// Return the default cache directory for factor-q.
///
/// Resolution order:
/// 1. `$XDG_CACHE_HOME/factor-q` if set
/// 2. `$HOME/.cache/factor-q` if set
/// 3. `<system temp dir>/factor-q` as a last resort
///
/// The third fallback matters in distroless and other minimal
/// containers where neither `HOME` nor `XDG_CACHE_HOME` is present and
/// cwd may not be writable. `env::temp_dir()` returns `/tmp` on Linux,
/// which is almost always writable even in stripped images. Operators
/// deploying factor-q should still prefer setting `FQ_CACHE_DIR`
/// explicitly to a mounted volume — the default only exists so a fresh
/// binary runs without any configuration.
pub fn default_cache_dir() -> PathBuf {
    resolve_cache_dir(
        std::env::var("XDG_CACHE_HOME").ok(),
        std::env::var("HOME").ok(),
        std::env::temp_dir(),
    )
}

/// Pure resolution of the cache directory, for testing.
fn resolve_cache_dir(xdg: Option<String>, home: Option<String>, temp_dir: PathBuf) -> PathBuf {
    if let Some(xdg) = xdg.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("factor-q");
    }
    if let Some(home) = home.filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".cache").join("factor-q");
    }
    temp_dir.join("factor-q")
}

/// Default path to the pricing JSON cache file.
pub fn default_pricing_cache_path() -> PathBuf {
    default_cache_dir().join("pricing.json")
}

/// One entry in the LiteLLM pricing JSON. We only read the fields we
/// care about; unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct LiteLlmEntry {
    input_cost_per_token: Option<f64>,
    #[serde(default)]
    max_input_tokens: Option<u32>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
}

/// Errors from loading or parsing pricing data.
#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error("failed to parse pricing JSON: {0}")]
    Parse(String),

    #[error("HTTP error fetching pricing: {0}")]
    Http(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const LITELLM_SAMPLE: &str = r#"{
        "sample_spec": {
            "max_tokens": "LEGACY",
            "input_cost_per_token": 0.0,
            "output_cost_per_token": 0.0
        },
        "claude-haiku-test": {
            "max_input_tokens": 200000,
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000005,
            "litellm_provider": "anthropic"
        },
        "claude-sonnet-test": {
            "input_cost_per_token": 0.000003,
            "output_cost_per_token": 0.000015,
            "cache_read_input_token_cost": 0.0000003,
            "cache_creation_input_token_cost": 0.00000375
        },
        "missing-prices": {
            "max_input_tokens": 4096
        }
    }"#;

    #[test]
    fn parses_litellm_entries() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        assert_eq!(table.len(), 2, "should skip sample_spec and missing-prices");

        let haiku = table.lookup("claude-haiku-test").unwrap();
        assert!((haiku.input_per_million - 1.0).abs() < 1e-9);
        assert!((haiku.output_per_million - 5.0).abs() < 1e-9);
        assert!(haiku.cache_read_per_million.is_none());

        let sonnet = table.lookup("claude-sonnet-test").unwrap();
        assert!((sonnet.input_per_million - 3.0).abs() < 1e-9);
        assert!((sonnet.output_per_million - 15.0).abs() < 1e-9);
        assert!((sonnet.cache_read_per_million.unwrap() - 0.3).abs() < 1e-9);
        assert!((sonnet.cache_write_per_million.unwrap() - 3.75).abs() < 1e-9);
    }

    #[test]
    fn context_window_reads_max_input_tokens() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        // Priced model that also lists a window.
        assert_eq!(table.context_window("claude-haiku-test"), Some(200_000));
        // A model skipped for pricing can still contribute a window.
        assert_eq!(table.context_window("missing-prices"), Some(4096));
        // A model with no window listed reads as unknown.
        assert_eq!(table.context_window("claude-sonnet-test"), None);
        // sample_spec is skipped entirely.
        assert_eq!(table.context_window("sample_spec"), None);
        // An unknown model is unknown.
        assert_eq!(table.context_window("nope"), None);
    }

    #[test]
    fn skips_sample_spec() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        assert!(table.lookup("sample_spec").is_none());
    }

    #[test]
    fn skips_entries_without_prices() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        assert!(table.lookup("missing-prices").is_none());
    }

    #[test]
    fn invalid_json_returns_error() {
        let err = PricingTable::from_litellm_json("not json").unwrap_err();
        assert!(matches!(err, PricingError::Parse(_)));
    }

    #[test]
    fn empty_table_lookups_return_none() {
        let table = PricingTable::empty();
        assert_eq!(table.len(), 0);
        assert!(table.lookup("anything").is_none());
    }

    #[test]
    fn calculate_matches_known_values() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
        };
        let (input, output, total) = pricing.calculate(&usage(100, 200, 0, 0));
        assert!((input - 0.0001).abs() < 1e-9);
        assert!((output - 0.001).abs() < 1e-9);
        assert!((total - 0.0011).abs() < 1e-9);
    }

    #[test]
    fn load_from_cache_falls_back_to_empty_on_missing_file() {
        // File doesn't exist — should return empty table without panicking.
        let path = PathBuf::from("/tmp/fq-nonexistent-pricing-cache-xyz-12345.json");
        let table = PricingTable::load_from_cache_or_empty(&path);
        assert!(table.is_empty());
    }

    #[test]
    fn load_from_cache_uses_cached_json_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        std::fs::write(&path, LITELLM_SAMPLE).unwrap();

        let table = PricingTable::load_from_cache_or_empty(&path);
        assert_eq!(table.len(), 2);
        assert!(table.lookup("claude-haiku-test").is_some());
    }

    #[test]
    fn insert_overrides_a_model_price() {
        let mut table = PricingTable::empty();
        table.insert(
            "custom/model",
            ModelPricing {
                input_per_million: 0.5,
                output_per_million: 1.5,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        );
        let p = table.lookup("custom/model").expect("inserted entry");
        assert_eq!(p.input_per_million, 0.5);
        assert_eq!(p.output_per_million, 1.5);
    }

    #[test]
    fn cache_age_of_a_fresh_file_is_small() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        std::fs::write(&path, "{}").unwrap();
        let age = cache_age(&path);
        assert!(age == "0m" || age == "just now", "unexpected age: {age}");
    }

    #[test]
    fn cache_age_of_a_missing_file_is_unknown() {
        let age = cache_age(Path::new("/tmp/factor-q-nonexistent-cache-xyz.json"));
        assert_eq!(age, "unknown age");
    }

    #[test]
    fn default_pricing_cache_path_is_usable() {
        // This test just exercises the function without asserting on the
        // host's env — it should never panic and always return a path.
        let path = default_pricing_cache_path();
        assert!(path.file_name().is_some());
        assert!(path.is_absolute(), "cache path should always be absolute");
    }

    #[test]
    fn resolve_cache_dir_prefers_xdg() {
        let dir = resolve_cache_dir(
            Some("/xdg/cache".to_string()),
            Some("/home/user".to_string()),
            PathBuf::from("/tmp"),
        );
        assert_eq!(dir, PathBuf::from("/xdg/cache/factor-q"));
    }

    #[test]
    fn resolve_cache_dir_falls_back_to_home_when_xdg_unset() {
        let dir = resolve_cache_dir(None, Some("/home/user".to_string()), PathBuf::from("/tmp"));
        assert_eq!(dir, PathBuf::from("/home/user/.cache/factor-q"));
    }

    #[test]
    fn resolve_cache_dir_falls_back_to_temp_dir_when_both_unset() {
        let dir = resolve_cache_dir(None, None, PathBuf::from("/tmp"));
        assert_eq!(dir, PathBuf::from("/tmp/factor-q"));
    }

    #[test]
    fn resolve_cache_dir_treats_empty_env_vars_as_unset() {
        // Empty env vars can occur in containers where something sets
        // HOME="" by mistake — treat them as unset, don't produce a
        // nonsensical "/factor-q" path.
        let dir = resolve_cache_dir(
            Some("".to_string()),
            Some("".to_string()),
            PathBuf::from("/tmp"),
        );
        assert_eq!(dir, PathBuf::from("/tmp/factor-q"));
    }

    /// Live network test: actually fetches LiteLLM's pricing JSON and
    /// confirms we can parse it. Gated on FQ_NETWORK_TESTS so CI without
    /// internet still passes.
    #[tokio::test]
    async fn fetches_and_parses_live_litellm_json() {
        if std::env::var("FQ_NETWORK_TESTS").is_err() {
            eprintln!("skipping: set FQ_NETWORK_TESTS=1 to run");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("pricing.json");

        let table = PricingTable::load(&cache_path).await;
        assert!(
            table.len() > 100,
            "expected hundreds of entries from LiteLLM, got {}",
            table.len()
        );

        // Spot-check a model we know LiteLLM ships.
        assert!(
            table.lookup("claude-sonnet-4-5").is_some()
                || table.lookup("claude-3-5-sonnet-latest").is_some(),
            "expected a Claude Sonnet entry"
        );

        // The cache file should have been written.
        assert!(cache_path.exists(), "cache file should have been written");
        let cached = std::fs::read_to_string(&cache_path).unwrap();
        assert!(cached.len() > 1000, "cached file should contain full JSON");
    }

    fn usage(input: u32, output: u32, read: u32, write: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
        }
    }

    #[test]
    fn calculate_without_cache_tokens_charges_base_rates() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        let sonnet = table.lookup("claude-sonnet-test").unwrap();
        // 100k in at $3/M + 1k out at $15/M.
        let (input, output, total) = sonnet.calculate(&usage(100_000, 1_000, 0, 0));
        assert!((input - 0.3).abs() < 1e-9);
        assert!((output - 0.015).abs() < 1e-9);
        assert!((total - 0.315).abs() < 1e-9);
    }

    #[test]
    fn calculate_prices_cache_reads_and_writes_separately() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        let sonnet = table.lookup("claude-sonnet-test").unwrap();
        // Total prompt 100k = 10k uncached + 70k read + 20k written.
        // 10k*$3/M + 70k*$0.3/M + 20k*$3.75/M = 0.03 + 0.021 + 0.075.
        let (input, _, total) = sonnet.calculate(&usage(100_000, 0, 70_000, 20_000));
        assert!((input - 0.126).abs() < 1e-9, "got {input}");
        assert!((total - 0.126).abs() < 1e-9);
        // Sanity: strictly cheaper than the same prompt uncached.
        let (uncached_input, _, _) = sonnet.calculate(&usage(100_000, 0, 0, 0));
        assert!(input < uncached_input);
    }

    #[test]
    fn calculate_falls_back_to_input_rate_without_cache_pricing() {
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        let haiku = table.lookup("claude-haiku-test").unwrap();
        assert!(haiku.cache_read_per_million.is_none());
        // Cache tokens with no published rates: charged at the base
        // input rate, i.e. identical to the fully-uncached cost.
        let (with_cache, _, _) = haiku.calculate(&usage(100_000, 0, 70_000, 20_000));
        let (without, _, _) = haiku.calculate(&usage(100_000, 0, 0, 0));
        assert!((with_cache - without).abs() < 1e-12);
    }

    #[test]
    fn calculate_saturates_when_cache_counts_exceed_total() {
        // Defensive: a provider reporting cache counts larger than the
        // prompt total must not underflow into a huge uncached count.
        let table = PricingTable::from_litellm_json(LITELLM_SAMPLE).unwrap();
        let sonnet = table.lookup("claude-sonnet-test").unwrap();
        let (input, _, _) = sonnet.calculate(&usage(1_000, 0, 2_000, 0));
        // 0 uncached + 2000 reads at $0.3/M.
        assert!((input - 0.0006).abs() < 1e-9, "got {input}");
    }
}
