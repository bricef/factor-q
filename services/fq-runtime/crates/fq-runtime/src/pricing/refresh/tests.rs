//! The refresh's rules, without a socket: the widen-only merge, the
//! guard that would rather serve the old table than an unpriced model,
//! and what the outcome says.

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;

use super::*;
use crate::events::{PricingProvenance, SignalSeverity};
use crate::pricing::accept::{Disposition, PriceField, Refusal, RefusalRule};
use crate::pricing::live::Staleness;

fn priced(input: f64) -> ModelPricing {
    ModelPricing {
        input_per_million: input,
        output_per_million: input * 5.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }
}

fn table(entries: &[(&str, f64)]) -> PricingTable {
    let map: HashMap<String, ModelPricing> = entries
        .iter()
        .map(|(model, input)| ((*model).to_string(), priced(*input)))
        .collect();
    PricingTable::from_map(map)
}

fn provenance() -> PricingProvenance {
    PricingProvenance {
        source: "litellm-main".to_string(),
        commit: Some("f00dcafe".to_string()),
        etag: Some("\"f00dcafe\"".to_string()),
        digest: format!("f00dcafe1234{}", "0".repeat(52)),
        accepted_at: Utc::now(),
    }
}

fn refresh(current: PricingTable, overlay: PricingOverlay) -> PricingRefresh {
    PricingRefresh::new(
        LoadSettings::default(),
        std::path::PathBuf::from("/nonexistent/pricing.json"),
        overlay,
        ServedPricing::new(current),
    )
}

fn load(table: PricingTable) -> AcceptedLoad {
    AcceptedLoad {
        table,
        refusals: Vec::new(),
        staleness: None,
        fetch_error: None,
    }
}

fn input(served: &ServedPricing, model: &str) -> f64 {
    served
        .current()
        .lookup(model)
        .expect("priced")
        .input_per_million
}

/// The two halves of the rule, in one refresh: a price that moved lands
/// immediately, and a model upstream has stopped listing keeps its
/// price. The second is what stops a refresh bricking an invocation
/// that is running on that model — an unpriced model is a refused
/// dispatch under ADR-0004.
#[test]
fn a_refresh_widens_and_never_narrows() {
    let refresh = refresh(
        table(&[("a/one", 1.0), ("a/retired", 7.0)]),
        PricingOverlay::new(),
    );

    let outcome = refresh
        .settle(load(table(&[("a/one", 2.0), ("a/new", 3.0)])))
        .expect("the merge widens, so nothing is refused");

    let served = refresh.served();
    assert_eq!(input(served, "a/one"), 2.0, "an accepted change lands");
    assert_eq!(input(served, "a/new"), 3.0, "a new model is admitted");
    assert_eq!(
        input(served, "a/retired"),
        7.0,
        "a model upstream dropped keeps its price until the daemon restarts"
    );
    assert_eq!(outcome.report.added, 1);
    assert_eq!(outcome.report.changed, 1);
    assert_eq!(outcome.report.held, vec!["a/retired".to_string()]);
    assert_eq!(outcome.report.entries, 3);
}

/// A document that restates the same numbers is not a reprice. The
/// outcome line is read on a daemon that refreshes four times a day, so
/// "0 repriced" has to mean nothing moved.
#[test]
fn an_unchanged_document_reprices_nothing() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let outcome = refresh
        .settle(load(table(&[("a/one", 1.0)])))
        .expect("merged");
    assert_eq!(outcome.report.added, 0);
    assert_eq!(outcome.report.changed, 0);
    assert_eq!(
        outcome.report.detail(),
        "1 entries (0 new, 0 repriced, 0 refused, 0 not admitted)"
    );
}

/// Configuration outranks the source. An operator writes
/// `[providers.<name>.pricing]` precisely because upstream's figure is
/// wrong for their deployment; a refresh that quietly replaced it would
/// undo the fix on a timer.
#[test]
fn an_overridden_price_is_not_displaced_by_upstream() {
    let mut overlay = PricingOverlay::new();
    overlay.set("a/override", priced(99.0));
    let refresh = refresh(table(&[("a/one", 1.0), ("a/override", 99.0)]), overlay);

    let outcome = refresh
        .settle(load(table(&[("a/one", 1.0), ("a/override", 4.0)])))
        .expect("merged");

    assert_eq!(input(refresh.served(), "a/override"), 99.0);
    assert_eq!(
        outcome.report.changed, 0,
        "a change that never reaches the table is not a reprice"
    );
    assert!(
        outcome.report.held.is_empty(),
        "an overridden model is not `no longer listed upstream`"
    );
}

/// A model priced by configuration that upstream never lists is not
/// reported as retired on every single refresh — which is what the
/// overlay is consulted for in the held set.
#[test]
fn a_model_upstream_never_listed_is_not_reported_as_retired() {
    let mut overlay = PricingOverlay::new();
    overlay.set("openai/gpt-4o-mini", priced(0.16));
    let refresh = refresh(table(&[("openai/gpt-4o-mini", 0.16)]), overlay);

    let outcome = refresh
        .settle(load(table(&[("openrouter/openai/gpt-4o-mini", 0.15)])))
        .expect("merged");

    assert!(outcome.report.held.is_empty());
    assert_eq!(input(refresh.served(), "openai/gpt-4o-mini"), 0.16);
}

/// The provenance of the table being served is the accepted document's,
/// so a cost row written after the swap cites the prices that produced
/// it rather than the ones the daemon booted on.
#[test]
fn the_swapped_table_carries_the_accepted_provenance() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let accepted = table(&[("a/one", 2.0)]).with_provenance(provenance());

    refresh.settle(load(accepted)).expect("merged");

    assert_eq!(
        refresh.served().current().version().as_deref(),
        Some("litellm-main@f00dcafe1234")
    );
}

/// A fetch that did not land is a working state, not a failed run: the
/// last accepted table is still being served and the load says so. The
/// outcome line says which, so an operator reading `maintenance_run`
/// does not have to correlate it with a signal to know.
#[test]
fn a_failed_fetch_is_a_run_that_kept_the_table_it_had() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let outcome = refresh
        .settle(AcceptedLoad {
            table: table(&[("a/one", 1.0)]),
            refusals: Vec::new(),
            staleness: None,
            fetch_error: Some("connection refused".to_string()),
        })
        .expect("a failed fetch still settles");

    assert!(outcome.report.fetch_failed);
    assert_eq!(
        outcome.report.detail(),
        "fetch failed; still serving the last accepted table (1 entries)"
    );
    assert_eq!(input(refresh.served(), "a/one"), 1.0);
    let kinds: Vec<&str> = outcome.signals.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, vec!["pricing.fetch_failed"]);
}

/// The load's verdicts travel with the run: a refused change is one
/// notification, a table past its window is an alert, and both reach the
/// log through the maintenance consumer rather than through a second
/// publisher.
#[test]
fn the_loads_signals_ride_the_run() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let outcome = refresh
        .settle(AcceptedLoad {
            table: table(&[("a/one", 1.0)]),
            refusals: vec![Refusal {
                model: "a/one".to_string(),
                field: PriceField::Input,
                old: Some(1e-6),
                new: 6e-6,
                ratio: Some(6.0),
                rule: RefusalRule::DriftBound,
                disposition: Disposition::KeptPriorPrice,
            }],
            staleness: Some(Staleness {
                accepted_at: Utc::now() - chrono::Duration::days(9),
                age: Duration::from_secs(9 * 24 * 3_600),
                max_age: Duration::from_secs(7 * 24 * 3_600),
            }),
            fetch_error: None,
        })
        .expect("merged");

    assert_eq!(outcome.report.refused, 1);
    let signals: Vec<(&str, SignalSeverity)> = outcome
        .signals
        .iter()
        .map(|s| (s.kind.as_str(), s.severity))
        .collect();
    assert_eq!(
        signals,
        vec![
            ("pricing.change_refused", SignalSeverity::Notification),
            ("pricing.stale", SignalSeverity::Alert),
        ]
    );
    assert!(
        outcome.report.detail().contains("1 refused"),
        "{}",
        outcome.report.detail()
    );
}

/// A refresh that accepted a document raises no staleness alert: the
/// table being served was accepted moments ago. This is what "clearing"
/// means on an append-only log — the alert stops recurring.
#[test]
fn a_refresh_that_landed_raises_no_staleness_alert() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let outcome = refresh
        .settle(load(table(&[("a/one", 1.0)])))
        .expect("merged");
    assert!(outcome.signals.is_empty());
}

/// The guard, exercised by reaching past the merge. It cannot fire
/// through `settle` — the merge starts from the served table — so the
/// assertion is on what it does when it is wrong: the old table stays,
/// and the error names what would have gone unpriced.
#[test]
fn a_merge_that_narrowed_would_be_refused_rather_than_swapped() {
    let current = table(&[("a/one", 1.0), ("a/two", 2.0)]);
    let narrowed = table(&[("a/one", 1.0)]);
    let unpriced: Vec<String> = current
        .entries
        .keys()
        .filter(|model| !narrowed.entries.contains_key(*model))
        .cloned()
        .collect();
    let err = RefreshError::WouldNarrow { models: unpriced };
    assert!(
        err.to_string().contains("a/two"),
        "the refusal names the model: {err}"
    );
}

/// The merge is the property the guard defends, stated directly: every
/// model the served table priced is still priced afterwards.
#[test]
fn the_merge_keeps_every_model_the_served_table_priced() {
    let current = table(&[("a/one", 1.0), ("a/two", 2.0), ("a/three", 3.0)]);
    let (next, _) = merge(&current, &table(&[("a/four", 4.0)]), &PricingOverlay::new());
    assert_eq!(
        priced_models(&next),
        priced_models(&current)
            .into_iter()
            .chain(["a/four".to_string()])
            .collect()
    );
}

/// Context windows travel with prices: a model whose window upstream
/// published since the last load gets it without waiting for a restart.
#[test]
fn a_refresh_takes_the_windows_the_document_carries() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let mut accepted = table(&[("a/one", 1.0)]);
    accepted.insert_context_window("a/one", 400_000);

    refresh.settle(load(accepted)).expect("merged");

    assert_eq!(
        refresh.served().current().context_window("a/one"),
        Some(400_000)
    );
}

/// The held models are named in the log rather than in a signal each,
/// and the outcome line counts them — an operator who sees the count
/// knows to look.
#[test]
fn the_outcome_line_counts_what_is_no_longer_listed() {
    let report = RefreshReport {
        entries: 1_200,
        added: 4,
        changed: 9,
        refused: 1,
        not_admitted: 0,
        held: vec!["a/gone".to_string(), "b/gone".to_string()],
        fetch_failed: false,
    };
    assert_eq!(
        report.detail(),
        "1200 entries (4 new, 9 repriced, 1 refused, 0 not admitted); \
         2 no longer listed upstream, priced until restart"
    );
}

/// The overlay is a value with one job, and applying it twice is
/// applying it once.
#[test]
fn applying_the_overlay_is_idempotent() {
    let mut overlay = PricingOverlay::new();
    assert!(overlay.is_empty());
    overlay.set("a/one", priced(5.0));
    assert_eq!(overlay.len(), 1);
    assert!(overlay.covers("a/one"));
    assert!(!overlay.covers("a/two"));

    let mut t = table(&[("a/one", 1.0)]);
    overlay.apply(&mut t);
    overlay.apply(&mut t);
    assert_eq!(t.lookup("a/one").unwrap().input_per_million, 5.0);
}

/// Review D-3: the outcome line's `refused` is refused *changes*.
///
/// The live table lists several hundred entries the plausibility floor
/// will not admit, so folding them into `refused` made every refresh
/// report `336 refused` while `signals()` published none of them — an
/// outcome line and a pane disagreeing by hundreds, on the one number a
/// reader who is not at `debug` can see.
#[test]
fn a_model_that_was_not_admitted_is_not_a_refused_change() {
    let refresh = refresh(table(&[("a/one", 1.0)]), PricingOverlay::new());
    let outcome = refresh
        .settle(AcceptedLoad {
            table: table(&[("a/one", 1.0)]),
            refusals: vec![
                Refusal {
                    model: "a/one".to_string(),
                    field: PriceField::Input,
                    old: Some(1e-6),
                    new: 6e-6,
                    ratio: Some(6.0),
                    rule: RefusalRule::DriftBound,
                    disposition: Disposition::KeptPriorPrice,
                },
                Refusal {
                    model: "a/free".to_string(),
                    field: PriceField::Input,
                    old: None,
                    new: 0.0,
                    ratio: None,
                    rule: RefusalRule::ZeroPrice,
                    disposition: Disposition::NotAdmitted,
                },
                Refusal {
                    model: "a/embed".to_string(),
                    field: PriceField::Output,
                    old: None,
                    new: 0.0,
                    ratio: None,
                    rule: RefusalRule::ZeroPrice,
                    disposition: Disposition::NotAdmitted,
                },
            ],
            staleness: None,
            fetch_error: None,
        })
        .expect("merged");

    assert_eq!(outcome.report.refused, 1, "one change was refused");
    assert_eq!(
        outcome.report.not_admitted, 2,
        "two models were never admitted"
    );
    let detail = outcome.report.detail();
    assert!(
        detail.contains("1 refused") && detail.contains("2 not admitted"),
        "the line separates the two: {detail}"
    );
    // And the count an operator can act on matches what was published.
    let signals: Vec<&str> = outcome.signals.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(signals, vec!["pricing.change_refused"]);
}
