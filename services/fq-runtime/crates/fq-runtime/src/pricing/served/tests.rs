use std::collections::HashMap;

use super::*;
use crate::pricing::ModelPricing;

fn priced(input: f64) -> ModelPricing {
    ModelPricing {
        input_per_million: input,
        output_per_million: input * 5.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
        cache_write_1h_per_million: None,
    }
}

fn table(entries: &[(&str, f64)]) -> PricingTable {
    let map: HashMap<String, ModelPricing> = entries
        .iter()
        .map(|(model, input)| ((*model).to_string(), priced(*input)))
        .collect();
    PricingTable::from_map(map)
}

/// The point of the handle: a holder taken before the swap reads the
/// table after it. Every consumer of the served table — the reducer
/// runner, the summariser — is constructed once and outlives every
/// refresh, so a handle that did not do this would make the refresh
/// inert.
#[test]
fn a_clone_taken_before_the_swap_reads_the_table_after_it() {
    let served = ServedPricing::new(table(&[("a/one", 1.0)]));
    let held = served.clone();

    let before = held.current();
    assert_eq!(before.lookup("a/one").unwrap().input_per_million, 1.0);
    assert!(before.lookup("a/two").is_none());

    served.swap(table(&[("a/one", 2.0), ("a/two", 3.0)]));

    let after = held.current();
    assert_eq!(after.lookup("a/one").unwrap().input_per_million, 2.0);
    assert_eq!(after.lookup("a/two").unwrap().input_per_million, 3.0);
}

/// A snapshot is whole and stays whole. The reason the handle holds an
/// `Arc<PricingTable>` rather than the table itself: a reader that has
/// priced a request against one table must not find the window or the
/// version belonging to another one a line later.
#[test]
fn a_snapshot_survives_the_swap_that_replaces_it() {
    let served = ServedPricing::new(table(&[("a/one", 1.0)]));
    let snapshot = served.current();

    let previous = served.swap(table(&[("a/one", 9.0)]));

    assert_eq!(snapshot.lookup("a/one").unwrap().input_per_million, 1.0);
    assert_eq!(previous.lookup("a/one").unwrap().input_per_million, 1.0);
    assert_eq!(
        served.current().lookup("a/one").unwrap().input_per_million,
        9.0
    );
}

/// The handle answers the same questions the `Arc<PricingTable>` it
/// replaced did, so the price path reads the same either side of a
/// refresh.
#[test]
fn the_handle_answers_for_the_table_it_holds() {
    let mut t = table(&[("a/one", 1.0)]);
    t.insert_context_window("a/one", 200_000);
    let served = ServedPricing::new(t);

    assert_eq!(served.len(), 1);
    assert!(!served.is_empty());
    let table = served.current();
    assert_eq!(table.context_window("a/one"), Some(200_000));
    assert_eq!(table.context_window("a/two"), None);
    assert!(table.version().is_none());
    assert!(served.provenance().is_none());
}

/// `Arc<PricingTable>` is what every existing construction site passes,
/// so the conversion is what keeps this a drop-in.
#[test]
fn an_arc_of_a_table_converts_into_a_handle() {
    let served: ServedPricing = Arc::new(table(&[("a/one", 4.0)])).into();
    assert_eq!(
        served.current().lookup("a/one").unwrap().output_per_million,
        20.0
    );
}

/// Concurrent readers and one writer: every read is a table that
/// existed, never a splice of two.
#[test]
fn readers_never_see_a_table_between_two_tables() {
    let served = ServedPricing::new(table(&[("a/one", 1.0), ("a/two", 1.0)]));
    let writer = served.clone();
    let handle = std::thread::spawn(move || {
        for generation in 2..200u32 {
            let price = f64::from(generation);
            writer.swap(table(&[("a/one", price), ("a/two", price)]));
        }
    });

    for _ in 0..2_000 {
        let snapshot = served.current();
        let one = snapshot.lookup("a/one").unwrap().input_per_million;
        let two = snapshot.lookup("a/two").unwrap().input_per_million;
        assert_eq!(
            one, two,
            "a reader saw one model's new price and not the other's"
        );
    }
    handle.join().unwrap();
}
