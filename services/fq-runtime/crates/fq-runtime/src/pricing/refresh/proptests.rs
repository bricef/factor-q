//! The refresh's one invariant, as a property rather than as the four
//! examples beside it (#344).
//!
//! **After any sequence of refreshes, every model that was priced is
//! still priced.** That is the whole safety claim: under ADR-0004 an
//! unpriced model is a *refused* dispatch, so the set of priced models
//! narrowing under a running daemon is the failure this module exists to
//! prevent — and it is a claim about a sequence, not about one merge,
//! because the table each refresh starts from is the one the last
//! refresh left.
//!
//! The generated documents are adversarial in exactly the way upstream
//! is: each one is an arbitrary subset of the model namespace at
//! arbitrary prices, so models appear, disappear and reappear across the
//! sequence, and the overlay covers an arbitrary slice of the namespace
//! on top.

use proptest::prelude::*;

use super::*;

/// Eight ids, so a generated document lists some and drops others.
const MODELS: [&str; 8] = [
    "a/one", "a/two", "b/one", "b/two", "c/one", "c/two", "d/one", "d/two",
];

fn price() -> impl Strategy<Value = f64> {
    (1u32..10_000u32).prop_map(|cents| f64::from(cents) / 100.0)
}

/// One accepted document: a subset of the namespace, priced.
fn document() -> impl Strategy<Value = PricingTable> {
    proptest::collection::vec((0usize..MODELS.len(), price()), 0..MODELS.len() + 2).prop_map(
        |entries| {
            let mut table = PricingTable::empty();
            for (index, input) in entries {
                table.insert(
                    MODELS[index],
                    ModelPricing {
                        input_per_million: input,
                        output_per_million: input * 5.0,
                        cache_read_per_million: None,
                        cache_write_per_million: None,
                        cache_write_1h_per_million: None,
                    },
                );
            }
            table
        },
    )
}

fn overlay() -> impl Strategy<Value = PricingOverlay> {
    proptest::collection::vec((0usize..MODELS.len(), price()), 0..3).prop_map(|entries| {
        let mut overlay = PricingOverlay::new();
        for (index, input) in entries {
            overlay.set(
                MODELS[index],
                ModelPricing {
                    input_per_million: input,
                    output_per_million: input * 5.0,
                    cache_read_per_million: None,
                    cache_write_per_million: None,
                    cache_write_1h_per_million: None,
                },
            );
        }
        overlay
    })
}

proptest! {
    /// The invariant: no sequence of refreshes ever unprices a model.
    ///
    /// Stated over the *declared* set as ADR-0004 does — whatever the
    /// daemon started with priced — because that is the set the startup
    /// guarantee checked and the set dispatch depends on.
    #[test]
    fn every_model_priced_at_start_is_priced_after_any_sequence_of_refreshes(
        start in document(),
        overlay in overlay(),
        documents in proptest::collection::vec(document(), 1..8),
    ) {
        let mut current = start.clone();
        overlay.apply(&mut current);
        let declared = priced_models(&current);

        for accepted in documents {
            let (next, _) = merge(&current, &accepted, &overlay);
            let priced = priced_models(&next);
            prop_assert!(
                declared.is_subset(&priced),
                "a refresh unpriced {:?}",
                declared.difference(&priced).collect::<Vec<_>>()
            );
            current = next;
        }
    }

    /// Configuration survives every refresh in the sequence, not just
    /// the first: the overlay is re-applied last each time, so upstream
    /// never wins on a later round.
    #[test]
    fn an_overlaid_price_survives_every_refresh(
        start in document(),
        overlay in overlay(),
        documents in proptest::collection::vec(document(), 1..8),
    ) {
        let mut current = start;
        overlay.apply(&mut current);

        for accepted in documents {
            let (next, _) = merge(&current, &accepted, &overlay);
            for (model, expected) in &overlay.entries {
                let served = next.lookup(model).expect("an overlaid model is priced");
                prop_assert_eq!(
                    served.input_per_million,
                    expected.input_per_million,
                    "upstream displaced the overlay for {}",
                    model
                );
            }
            current = next;
        }
    }

    /// The priced set only ever grows within a daemon's lifetime, which
    /// is the merge's defining property and the reason the swap needs no
    /// coordination with anything in flight.
    #[test]
    fn the_priced_set_is_monotonic(
        start in document(),
        documents in proptest::collection::vec(document(), 1..8),
    ) {
        let mut current = start;
        for accepted in documents {
            let before = priced_models(&current);
            let (next, report) = merge(&current, &accepted, &PricingOverlay::new());
            let after = priced_models(&next);
            prop_assert!(before.is_subset(&after));
            prop_assert_eq!(after.len(), before.len() + report.added);
            current = next;
        }
    }
}
