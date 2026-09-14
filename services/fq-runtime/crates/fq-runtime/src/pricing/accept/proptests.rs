//! The acceptance rules as properties, not as fifteen examples (#735).
//!
//! The examples beside this file are points in a space; this is the
//! space. Two arbitrary tables — a last-accepted one and a candidate —
//! are put through [`accept`], and the guarantees the daemon's spend
//! depends on are checked over the whole accepted result:
//!
//! 1. **Nothing moves further than the bound.** Every accepted price is
//!    either exactly its prior price (the model was refused and
//!    reverted) or within `max_drift_ratio` of a *usable* prior — one
//!    that passes the plausibility floor. A model whose prior does not
//!    is judged at admission instead, where the only bound is
//!    plausibility: there is no ratio to a zero.
//! 2. **No accepted price is zero, full stop.** Not a change to zero,
//!    not a new model at zero, and not a zero inherited from a prior
//!    table — a model tracking at $0 defeats every budget, which is the
//!    failure ADR-0004's guarantee exists to prevent.
//! 3. **A new model is plausible or absent.** Every accepted model with
//!    no usable prior carries a positive price on every category it
//!    reports.
//! 4. **A refusal is a decision about a model, not a mutation.** Every
//!    refused model is either at its prior price or gone — reverted when
//!    the refusal names a prior price, dropped when it does not — and no
//!    model is refused twice: the notification is one per model per
//!    load.
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

/// The prior price a move is bounded against, when there is one: a prior
/// entry that fails the plausibility floor is not a price, so the model
/// is judged at admission instead. The same filter [`accept`] applies.
fn usable_prior<'a>(prior: &'a PricingTable, model: &str) -> Option<&'a ModelPricing> {
    prior
        .lookup(model)
        .filter(|pricing| judge_admission(model, pricing).is_none())
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
            // Rule 2: no accepted price is zero, whatever route the model
            // took into the table.
            for (field, new) in prices(now) {
                prop_assert!(new > 0.0, "{model} {field} is accepted at {new}");
            }
            // A prior that fails the floor is not a prior: the model was
            // judged at admission, where rule 2 above is the whole rule.
            let Some(before) = usable_prior(&prior, model) else { continue };
            let reverted = prices(now) == prices(before);
            for (field, new) in prices(now) {
                let Some(old) = field.per_million(before) else {
                    // A category the prior did not report: judged for
                    // plausibility only, which rule 2 already asserted.
                    continue;
                };
                // Rule 1: nothing moves further than the bound. A usable
                // prior is positive on every category it reports, so
                // every accepted move has a ratio.
                if !reverted {
                    let ratio = (new / old).max(old / new);
                    prop_assert!(
                        ratio <= max_drift_ratio,
                        "{model} {field} moved {ratio}x, past {max_drift_ratio}",
                    );
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
            if refusal.is_admission() {
                prop_assert!(
                    accepted.lookup(&refusal.model).is_none(),
                    "a model refused at admission must be absent",
                );
            } else {
                let before = usable_prior(&prior, &refusal.model)
                    .expect("a refused change names the prior it reverted to");
                prop_assert_eq!(
                    prices(accepted.lookup(&refusal.model).expect("reverted, not dropped")),
                    prices(before),
                    "a refused change must revert the model whole",
                );
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
