//! The acceptance rules as properties, not as fifteen examples (#735).
//!
//! The examples beside this file are points in a space; this is the
//! space. Two arbitrary tables — a last-accepted one and a candidate —
//! are put through [`accept`], and the guarantees the daemon's spend
//! depends on are checked over the whole accepted result:
//!
//! 1. **Nothing moves further than the bound.** Every accepted price is
//!    either exactly its prior price (the model was refused and
//!    reverted) or within `max_drift_ratio` of it.
//! 2. **Nothing that cost money becomes free.** No accepted price is
//!    zero where the prior was positive — the failure ADR-0004's
//!    guarantee exists to prevent, since a model tracking at $0 defeats
//!    every budget.
//! 3. **A new model is plausible or absent.** Every accepted model with
//!    no prior carries a positive price on every category it reports.
//! 4. **A refusal is a decision about a model, not a mutation.** Every
//!    refused model is either at its prior price or gone, and no model
//!    is refused twice — the notification is one per model per load.
//!
//! Prices are drawn from a range that includes zero and spans four
//! orders of magnitude, so both rules fire often; the generated tables
//! share a model namespace of six ids, so the prior/candidate overlap —
//! the interesting case — is dense rather than accidental.

use proptest::prelude::*;

use super::*;

/// Six ids, so two independently generated tables overlap on most of
/// them. Real drift is a change to a model both documents list.
const MODELS: [&str; 6] = ["a/one", "a/two", "b/one", "b/two", "c/one", "c/two"];

/// Per-million prices spanning four orders of magnitude, with zero and
/// the fringe values that acceptance is supposed to catch reachable.
fn price() -> impl Strategy<Value = f64> {
    prop_oneof![
        1 => Just(0.0),
        8 => (1u32..10_000u32).prop_map(|cents| f64::from(cents) / 100.0),
    ]
}

fn model_pricing() -> impl Strategy<Value = ModelPricing> {
    (
        price(),
        price(),
        proptest::option::of(price()),
        proptest::option::of(price()),
    )
        .prop_map(|(input, output, cache_read, cache_write)| ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cache_read_per_million: cache_read,
            cache_write_per_million: cache_write,
        })
}

fn pricing_table() -> impl Strategy<Value = PricingTable> {
    proptest::collection::vec(proptest::option::of(model_pricing()), MODELS.len()).prop_map(
        |entries| {
            let mut table = PricingTable::empty();
            for (model, pricing) in MODELS.iter().zip(entries) {
                if let Some(pricing) = pricing {
                    table.insert(*model, pricing);
                }
            }
            table
        },
    )
}

/// Every price a model reports, paired with its field, in the units
/// acceptance judges in.
fn prices(pricing: &ModelPricing) -> Vec<(PriceField, f64)> {
    PriceField::ORDER
        .iter()
        .filter_map(|field| field.per_million(pricing).map(|price| (*field, price)))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn accepted_prices_never_break_the_rules(
        prior in pricing_table(),
        candidate in pricing_table(),
        max_drift_ratio in 1.5f64..20.0,
    ) {
        let rules = AcceptanceRules { max_drift_ratio };
        let (accepted, refusals) = accept(&prior, candidate.clone(), rules);

        for model in MODELS {
            let Some(now) = accepted.lookup(model) else { continue };
            match prior.lookup(model) {
                Some(before) => {
                    let reverted = prices(now) == prices(before);
                    for (field, new) in prices(now) {
                        let Some(old) = field.per_million(before) else {
                            // A category the prior did not report: judged
                            // for plausibility only, and it must be
                            // plausible.
                            prop_assert!(new > 0.0, "{model} {field} was admitted at {new}");
                            continue;
                        };
                        // Rule 2: nothing that cost money becomes free.
                        prop_assert!(
                            old <= 0.0 || new > 0.0,
                            "{model} {field} went from {old} to {new}",
                        );
                        // Rule 1: nothing moves further than the bound.
                        if old > 0.0 && new > 0.0 && !reverted {
                            let ratio = (new / old).max(old / new);
                            prop_assert!(
                                ratio <= max_drift_ratio,
                                "{model} {field} moved {ratio}x, past {max_drift_ratio}",
                            );
                        }
                        if old <= 0.0 {
                            prop_assert!(
                                reverted || new <= 0.0,
                                "{model} {field} moved off zero to {new} without reverting",
                            );
                        }
                    }
                }
                None => {
                    // Rule 3: a new model is plausible or absent.
                    for (field, new) in prices(now) {
                        prop_assert!(
                            new > 0.0,
                            "new model {model} was admitted with {field} = {new}",
                        );
                    }
                }
            }
        }

        // Rule 4: one refusal per model, and each names a model the
        // candidate proposed.
        let mut refused: Vec<&str> = refusals.iter().map(|r| r.model.as_str()).collect();
        let count = refused.len();
        refused.sort_unstable();
        refused.dedup();
        prop_assert_eq!(count, refused.len(), "a model was refused twice");
        for refusal in &refusals {
            prop_assert!(candidate.lookup(&refusal.model).is_some());
            match prior.lookup(&refusal.model) {
                Some(before) => prop_assert_eq!(
                    prices(accepted.lookup(&refusal.model).expect("reverted, not dropped")),
                    prices(before),
                    "a refused change must revert the model whole",
                ),
                None => prop_assert!(
                    accepted.lookup(&refusal.model).is_none(),
                    "a model refused at admission must be absent",
                ),
            }
        }
    }

    /// Acceptance is idempotent: feeding the accepted table back as the
    /// candidate refuses nothing and changes nothing. Without this a
    /// refusal could oscillate — the daemon writing one table at
    /// startup and a different one on the next load from the same
    /// source.
    #[test]
    fn accepting_what_was_accepted_is_a_no_op(
        prior in pricing_table(),
        candidate in pricing_table(),
    ) {
        let rules = AcceptanceRules::default();
        let (accepted, _) = accept(&prior, candidate, rules);
        let (again, refusals) = accept(&accepted, accepted.clone(), rules);
        prop_assert!(refusals.is_empty(), "{refusals:?}");
        for model in MODELS {
            prop_assert_eq!(
                again.lookup(model).map(prices),
                accepted.lookup(model).map(prices),
            );
        }
    }
}
