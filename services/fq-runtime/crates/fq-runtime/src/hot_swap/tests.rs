use super::*;

/// A stand-in for the registries and tables this cell really holds. A
/// named struct rather than a `Vec`: `Arc<[T]>` and `Arc<T>` are both
/// reachable from a `Vec<T>`, so the generic `Into<Arc<T>>` argument
/// cannot be inferred for one — an ambiguity no real holder has, since
/// none of them is a slice or a `str`.
#[derive(Debug, PartialEq, Eq)]
struct Generation(Vec<u32>);

impl Generation {
    fn of(value: u32) -> Self {
        Self(vec![value; 8])
    }
}

/// The property every holder of one of these relies on: a snapshot
/// taken before a swap is still the value it was, and the swap returns
/// what it replaced.
#[test]
fn a_snapshot_survives_the_swap_that_replaces_it() {
    let cell = HotSwap::new(Generation::of(1));
    let snapshot = cell.current();

    let previous = cell.swap(Generation::of(9));

    assert_eq!(*snapshot, Generation::of(1), "the snapshot is whole");
    assert_eq!(
        *previous,
        Generation::of(1),
        "the swap hands back what it replaced"
    );
    assert_eq!(
        *cell.current(),
        Generation::of(9),
        "and the cell holds the new value"
    );
}

/// Clones share one cell, which is what makes this a handle rather than
/// a container: a holder constructed before a swap reads the value
/// after it.
#[test]
fn a_clone_taken_before_the_swap_reads_the_value_after_it() {
    let cell = HotSwap::new(Generation::of(1));
    let held = cell.clone();

    cell.swap(Generation::of(2));

    assert_eq!(*held.current(), Generation::of(2));
}

/// Concurrent readers and one writer: every read is a value that
/// existed, never a splice of two.
#[test]
fn readers_never_see_a_value_between_two_values() {
    let cell = HotSwap::new(Generation::of(0));
    let writer = cell.clone();
    let handle = std::thread::spawn(move || {
        for generation in 1..500u32 {
            writer.swap(Generation::of(generation));
        }
    });
    for _ in 0..2_000 {
        let seen = cell.current();
        assert!(
            seen.0.iter().all(|entry| *entry == seen.0[0]),
            "a reader saw a value half way through a swap: {seen:?}"
        );
    }
    handle.join().expect("writer");
}
