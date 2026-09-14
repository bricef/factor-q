use std::collections::HashMap;

use chrono::TimeZone;
use serde_json::json;

use super::*;
use crate::events::SignalSeverity;

/// A LiteLLM-shaped document with the fields the table reads and one it
/// does not, so the splice can be checked for faithfulness.
fn document(entries: &[(&str, f64, f64)]) -> String {
    let map: serde_json::Map<String, Value> = entries
        .iter()
        .map(|(model, input, output)| {
            (
                (*model).to_string(),
                json!({
                    "input_cost_per_token": input,
                    "output_cost_per_token": output,
                    "litellm_provider": "somebody",
                    "max_input_tokens": 200000,
                }),
            )
        })
        .collect();
    serde_json::to_string_pretty(&map).unwrap()
}

fn fetched(body: String) -> Result<FetchedDocument, PricingError> {
    Ok(FetchedDocument {
        body,
        etag: Some("etag-of-the-blob".to_string()),
    })
}

fn settings() -> LoadSettings {
    LoadSettings::default()
}

struct Cache {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn cache() -> Cache {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing.json");
    Cache { _dir: dir, path }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0).unwrap()
}

fn input_per_million(table: &PricingTable, model: &str) -> f64 {
    table.lookup(model).expect("priced").input_per_million
}

/// The first load has nothing to compare against: everything plausible
/// in the document is accepted, and the cache is written with its
/// provenance.
#[test]
fn a_first_load_accepts_the_document_and_records_its_provenance() {
    let cache = cache();
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        Some("f00dcafe0000000000000000000000000000abcd".to_string()),
        now(),
    );

    assert!(load.refusals.is_empty());
    assert!(load.fetch_error.is_none());
    assert_eq!(input_per_million(&load.table, "a/one"), 1.0);

    let provenance = load.table.provenance().expect("stamped");
    assert_eq!(provenance.source, "litellm-main");
    assert_eq!(
        provenance.commit.as_deref(),
        Some("f00dcafe0000000000000000000000000000abcd")
    );
    assert_eq!(
        provenance.etag.as_deref(),
        Some("etag-of-the-blob"),
        "the blob's ETag is recorded as an ETag, beside the commit"
    );
    assert_eq!(provenance.accepted_at, now());
    // The digest names exactly the bytes in the cache.
    let bytes = std::fs::read(&cache.path).unwrap();
    assert_eq!(provenance.digest, digest_of(&bytes));
    assert_eq!(provenance.digest.len(), 64);
    // ... and the sidecar says the same thing.
    let sidecar: PricingProvenance =
        serde_json::from_slice(&std::fs::read(provenance_path(&cache.path)).unwrap()).unwrap();
    assert_eq!(&sidecar, provenance);
}

/// #735 acceptance box 1, end to end: the refused model is served at its
/// prior price, the accepted one at its new price, and the cache holds
/// what was accepted rather than what was fetched.
#[test]
fn the_cache_holds_the_accepted_table_not_the_fetched_one() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6), ("a/two", 1e-6, 5e-6)])),
        None,
        now(),
    );

    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 6e-6, 5e-6), ("a/two", 2e-6, 5e-6)])),
        None,
        now(),
    );

    assert_eq!(input_per_million(&load.table, "a/one"), 1.0);
    assert_eq!(input_per_million(&load.table, "a/two"), 2.0);
    assert_eq!(load.refusals.len(), 1);

    // The next load compares against what was accepted: the same 6x
    // document is refused again rather than becoming the new baseline.
    let cached: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&cache.path).unwrap()).unwrap();
    assert_eq!(cached["a/one"]["input_cost_per_token"], json!(1e-6));
    assert_eq!(cached["a/two"]["input_cost_per_token"], json!(2e-6));
    assert_eq!(
        cached["a/one"]["litellm_provider"],
        json!("somebody"),
        "the splice keeps upstream's own fields"
    );
}

/// The digest is a function of the prices, so a daemon that restarts
/// onto an unchanged document writes the same version — the property
/// that makes "did the prices move between these runs?" answerable.
#[test]
fn an_unchanged_document_reloads_to_the_same_version() {
    let cache = cache();
    let doc = document(&[("a/one", 1e-6, 5e-6)]);
    let first = settle(&settings(), &cache.path, fetched(doc.clone()), None, now());
    let later = now() + chrono::Duration::hours(3);
    let second = settle(&settings(), &cache.path, fetched(doc), None, later);

    assert_eq!(first.table.version(), second.table.version());
    assert_ne!(
        first.table.provenance().unwrap().accepted_at,
        second.table.provenance().unwrap().accepted_at,
        "the same version, freshly accepted"
    );
}

/// #735 acceptance box 2, at the load path: a new zero-priced model
/// never enters the table, so ADR-0004's at-use backstop is what refuses
/// the dispatch.
#[test]
fn a_new_zero_priced_model_is_not_admitted_and_is_not_cached() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        None,
        now(),
    );
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6), ("free/model", 0.0, 0.0)])),
        None,
        now(),
    );

    assert!(load.table.lookup("free/model").is_none());
    let cached: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&cache.path).unwrap()).unwrap();
    assert!(!cached.contains_key("free/model"));
    assert_eq!(load.refusals.len(), 1);
    assert!(load.refusals[0].is_admission());

    assert!(
        load.signals().is_empty(),
        "an admission refusal is recorded and logged, not published"
    );
}

/// The live table lists several hundred free, local and embedding
/// entries priced at zero, so on a first start every one of them fails
/// the admission floor. **None of them is published.** A pane with three
/// hundred identical lines is a pane nobody reads, and the fact is a
/// standing property of the source rather than something that happened;
/// the moment one of them matters — something declares it — ADR-0004's
/// startup guarantee refuses to run and names it.
#[test]
fn hundreds_of_unadmitted_models_publish_nothing() {
    let cache = cache();
    let free: Vec<(&str, f64, f64)> = FREE_MODELS.iter().map(|model| (*model, 0.0, 0.0)).collect();
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&free)),
        None,
        now(),
    );

    assert_eq!(
        load.refusals.len(),
        FREE_MODELS.len(),
        "every one is recorded on the load, for the log and for a reader"
    );
    assert!(load.signals().is_empty(), "and none is published");
    assert!(load.table.is_empty(), "and none is priced");
}

/// Thirty ids, enough to exceed the naming cap.
const FREE_MODELS: [&str; 30] = [
    "free/a01", "free/a02", "free/a03", "free/a04", "free/a05", "free/a06", "free/a07", "free/a08",
    "free/a09", "free/a10", "free/a11", "free/a12", "free/a13", "free/a14", "free/a15", "free/a16",
    "free/a17", "free/a18", "free/a19", "free/a20", "free/a21", "free/a22", "free/a23", "free/a24",
    "free/a25", "free/a26", "free/a27", "free/a28", "free/a29", "free/a30",
];

/// The migration case, and the one that matters for the deployment that
/// exists: before acceptance shipped, the cache was the **raw fetched
/// document** — several hundred free, local and embedding entries at $0 —
/// and it has no provenance sidecar. Those entries are not a prior price
/// to bound anything against, so the first load under acceptance judges
/// each of them at admission and drops it, rather than laundering the $0
/// into the accepted table and serving it as a price for ever.
#[test]
fn a_raw_cache_without_a_sidecar_does_not_launder_its_zero_prices() {
    let cache = cache();
    // Written by the old loader: the fetched document, verbatim, with no
    // sidecar beside it.
    std::fs::write(
        &cache.path,
        document(&[("free/model", 0.0, 0.0), ("a/one", 1e-6, 5e-6)]),
    )
    .unwrap();
    assert!(!provenance_path(&cache.path).exists());

    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("free/model", 0.0, 0.0), ("a/one", 1e-6, 5e-6)])),
        None,
        now(),
    );

    assert!(
        load.table.lookup("free/model").is_none(),
        "a zero-priced cache entry must not become an accepted price"
    );
    assert_eq!(input_per_million(&load.table, "a/one"), 1.0);
    let cached: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&cache.path).unwrap()).unwrap();
    assert!(
        !cached.contains_key("free/model"),
        "and the rewritten cache must not hold it either"
    );
    assert_eq!(load.refusals.len(), 1);
    assert!(load.refusals[0].is_admission());
    assert!(
        load.signals().is_empty(),
        "a standing property of the source, not news"
    );
}

/// A model that was free upstream and now costs money is admitted at the
/// new price on the next load — and the cache carries the new figure, so
/// the load after that bounds against it like any other price.
#[test]
fn a_cached_zero_price_that_starts_costing_money_is_admitted() {
    let cache = cache();
    std::fs::write(&cache.path, document(&[("free/model", 0.0, 0.0)])).unwrap();

    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("free/model", 1e-6, 5e-6)])),
        None,
        now(),
    );

    assert_eq!(input_per_million(&load.table, "free/model"), 1.0);
    assert!(load.refusals.is_empty(), "{:?}", load.refusals);
    let cached: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&cache.path).unwrap()).unwrap();
    assert_eq!(cached["free/model"]["input_cost_per_token"], json!(1e-6));
}

/// Rule 6: a failed fetch keeps last-known-good, says so, and is a
/// notification rather than an alert.
#[test]
fn a_failed_fetch_serves_the_last_accepted_table() {
    let cache = cache();
    let first = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        Some("f00dcafe0000000000000000000000000000abcd".to_string()),
        now(),
    );

    let load = settle(
        &settings(),
        &cache.path,
        Err(PricingError::Http("connection refused".to_string())),
        None,
        now() + chrono::Duration::hours(1),
    );

    assert_eq!(input_per_million(&load.table, "a/one"), 1.0);
    assert!(load.fetch_error.is_some());
    assert!(load.staleness.is_none(), "an hour is not a week");
    assert_eq!(
        load.table.version(),
        first.table.version(),
        "the served table keeps the provenance it was accepted with"
    );
    let signals = load.signals();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].kind().as_str(), "pricing.fetch_failed");
    assert_eq!(signals[0].payload.severity, SignalSeverity::Notification);
}

/// A load where the commits API did not answer records no commit at
/// all: the `ETag` is a blob identifier, and offering it as a commit
/// would send a reader looking for a sha that upstream never had.
#[test]
fn a_load_with_no_commit_records_the_etag_and_no_commit() {
    let cache = cache();
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        None,
        now(),
    );

    let provenance = load.table.provenance().expect("stamped");
    assert_eq!(provenance.commit, None);
    assert_eq!(provenance.etag.as_deref(), Some("etag-of-the-blob"));
    // The digest is what identifies the table either way.
    assert_eq!(provenance.digest.len(), 64);
}

/// A document that does not parse is a failed fetch by another name.
#[test]
fn a_malformed_document_keeps_the_last_accepted_table() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        None,
        now(),
    );
    let load = settle(
        &settings(),
        &cache.path,
        fetched("<html>502 Bad Gateway</html>".to_string()),
        None,
        now(),
    );
    assert_eq!(input_per_million(&load.table, "a/one"), 1.0);
    assert!(load.fetch_error.is_some());
}

/// With no cache and no fetch there is nothing to serve. The daemon does
/// not fail here — ADR-0004's startup guarantee is what refuses to run.
#[test]
fn no_fetch_and_no_cache_is_an_empty_table_and_a_notification() {
    let cache = cache();
    let load = settle(
        &settings(),
        &cache.path,
        Err(PricingError::Http("connection refused".to_string())),
        None,
        now(),
    );
    assert!(load.table.is_empty());
    assert!(load.table.provenance().is_none());
    assert!(load.staleness.is_none(), "there is no table to be stale");
    assert_eq!(load.signals().len(), 1);
}

/// Rule 6's second half: a table that has not refreshed inside the
/// window is an alert, and only the fallback path can raise one — a
/// table accepted moments ago is not old.
#[test]
fn a_table_past_its_window_alerts() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        None,
        now(),
    );
    let much_later = now() + chrono::Duration::days(9);
    let load = settle(
        &settings(),
        &cache.path,
        Err(PricingError::Http("connection refused".to_string())),
        None,
        much_later,
    );

    let stale = load.staleness.as_ref().expect("nine days is past seven");
    assert_eq!(stale.accepted_at, now());
    assert_eq!(stale.max_age, DEFAULT_MAX_AGE);
    let signals = load.signals();
    let alert = signals
        .iter()
        .find(|s| s.kind().as_str() == "pricing.stale")
        .expect("a stale alert");
    assert_eq!(alert.payload.severity, SignalSeverity::Alert);
    assert_eq!(alert.payload.detail["window_hours"], json!(168));
    assert_eq!(
        alert.payload.detail["last_refresh_ms"],
        json!(now().timestamp_millis())
    );
}

/// A hand-edited cache is not the table its sidecar describes, so the
/// provenance is dropped rather than believed.
#[test]
fn a_cache_that_does_not_match_its_digest_loses_its_provenance() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        None,
        now(),
    );
    std::fs::write(&cache.path, document(&[("a/one", 2e-6, 5e-6)])).unwrap();

    let load = settle(
        &settings(),
        &cache.path,
        Err(PricingError::Http("offline".to_string())),
        None,
        now(),
    );
    assert_eq!(
        input_per_million(&load.table, "a/one"),
        2.0,
        "the file on disk is still the table"
    );
    assert!(
        load.table.provenance().is_none(),
        "but nothing is claimed about where it came from"
    );
}

/// The refusal notification carries what the operator-signal registry
/// says it does, in the units the source states prices in.
#[test]
fn a_refusal_signal_names_model_field_old_new_ratio_and_rule() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("moonshotai/kimi-k3", 6e-7, 2.5e-6)])),
        None,
        now(),
    );
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("moonshotai/kimi-k3", 3.7e-6, 2.5e-6)])),
        None,
        now(),
    );

    let signals = load.signals();
    assert_eq!(signals.len(), 1, "one notification per model per load");
    let signal = &signals[0];
    assert_eq!(signal.kind().as_str(), "pricing.change_refused");
    assert_eq!(signal.payload.severity, SignalSeverity::Notification);
    assert_eq!(
        signal.payload.summary,
        "moonshotai/kimi-k3 input_cost_per_token moved 6.2x; kept the prior price"
    );
    assert_eq!(signal.payload.detail["model"], json!("moonshotai/kimi-k3"));
    assert_eq!(
        signal.payload.detail["field"],
        json!("input_cost_per_token")
    );
    // Per token, as the source states prices — within the rounding a
    // trip through per-million and back costs.
    assert!((signal.payload.detail["old"].as_f64().unwrap() - 6e-7).abs() < 1e-18);
    assert!((signal.payload.detail["new"].as_f64().unwrap() - 3.7e-6).abs() < 1e-18);
    assert_eq!(signal.payload.detail["rule"], json!("drift_bound"));
    let ratio = signal.payload.detail["ratio"].as_f64().unwrap();
    assert!((ratio - 6.166).abs() < 0.01, "{ratio}");
}

/// Several refused models are several notifications, one each — never
/// one for the load.
#[test]
fn each_refused_model_gets_its_own_notification() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6), ("a/two", 1e-6, 5e-6)])),
        None,
        now(),
    );
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 9e-6, 5e-6), ("a/two", 0.0, 5e-6)])),
        None,
        now(),
    );
    let signals = load.signals();
    assert_eq!(signals.len(), 2);
    assert!(
        signals
            .iter()
            .all(|s| s.kind().as_str() == "pricing.change_refused")
    );
}

/// A pin chooses the *document*, never the rules. A pinned commit whose
/// copy of the table moves a price 6x is refused exactly as `main`'s
/// would be — the discipline is on acceptance, and the source setting is
/// upstream of it rather than an exemption from it.
#[test]
fn a_pinned_source_is_judged_by_the_same_rules() {
    let cache = cache();
    let pinned = LoadSettings {
        source: TableSource::Pinned("f00dcafe".to_string()),
        ..LoadSettings::default()
    };
    settle(
        &pinned,
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6)])),
        pinned.source.commit().map(str::to_string),
        now(),
    );

    let load = settle(
        &pinned,
        &cache.path,
        fetched(document(&[("a/one", 6e-6, 5e-6)])),
        pinned.source.commit().map(str::to_string),
        now(),
    );

    assert_eq!(
        input_per_million(&load.table, "a/one"),
        1.0,
        "a pin is not an exemption from the bound"
    );
    assert_eq!(load.refusals.len(), 1);
    assert!(!load.refusals[0].is_admission());
    let signals = load.signals();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].kind().as_str(), "pricing.change_refused");
    // ... and the provenance still says which pin it was.
    let provenance = load.table.provenance().expect("stamped");
    assert_eq!(provenance.source, "pinned:f00dcafe");
    assert_eq!(provenance.commit.as_deref(), Some("f00dcafe"));
}

#[test]
fn the_source_setting_round_trips() {
    assert_eq!(
        TableSource::parse("litellm-main").unwrap(),
        TableSource::LitellmMain
    );
    assert_eq!(
        TableSource::LitellmMain.url(),
        super::super::LITELLM_PRICING_URL
    );
    assert!(TableSource::parse("pinned:0123456").is_ok());
    assert!(TableSource::parse("pinned:012345").is_err(), "too short");
    assert!(TableSource::parse("litellm").is_err());
}

/// The digest must not depend on the order two documents were spliced
/// in — `serde_json`'s map is sorted, and this is the assertion that
/// notices if that ever stops being true.
#[test]
fn the_digest_is_independent_of_key_order() {
    let one: serde_json::Map<String, Value> =
        serde_json::from_str(r#"{"a": {"x": 1}, "b": {"y": 2}}"#).unwrap();
    let other: serde_json::Map<String, Value> =
        serde_json::from_str(r#"{"b": {"y": 2}, "a": {"x": 1}}"#).unwrap();
    assert_eq!(
        digest_of(&canonical_bytes(&one)),
        digest_of(&canonical_bytes(&other))
    );
}

/// The table the daemon serves and the bytes it cached have to agree —
/// re-reading the cache must reproduce the accepted prices exactly.
#[test]
fn the_cached_bytes_reparse_to_the_table_that_was_served() {
    let cache = cache();
    settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 1e-6, 5e-6), ("a/two", 3e-6, 9e-6)])),
        None,
        now(),
    );
    let load = settle(
        &settings(),
        &cache.path,
        fetched(document(&[("a/one", 9e-6, 5e-6), ("a/two", 4e-6, 9e-6)])),
        None,
        now(),
    );

    let reloaded =
        PricingTable::from_litellm_json(&std::fs::read_to_string(&cache.path).unwrap()).unwrap();
    let served: HashMap<&str, f64> = ["a/one", "a/two"]
        .iter()
        .map(|m| (*m, input_per_million(&load.table, m)))
        .collect();
    for (model, price) in served {
        assert_eq!(input_per_million(&reloaded, model), price, "{model}");
    }
}
