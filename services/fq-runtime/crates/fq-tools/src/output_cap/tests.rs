use proptest::prelude::*;

use super::*;

/// The longest the marker can be for an output of `total` bytes under
/// `max_bytes` — what [`cap_output`] reserves.
fn reserve(total: usize, max_bytes: usize) -> usize {
    truncation_marker(total, total, max_bytes).len()
}

/// The kept-byte count a capped result's marker states.
fn stated_kept(capped: &str) -> usize {
    let (_, tail) = capped
        .rsplit_once("kept the first ")
        .expect("a capped result carries the marker");
    tail.split(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("the marker states the kept count")
}

#[test]
fn output_at_or_under_the_cap_is_untouched() {
    let cap = 1024;
    for len in [0, 1, cap - 1, cap] {
        let output = "x".repeat(len);
        assert_eq!(cap_output(output.clone(), cap), output, "len {len}");
    }
}

#[test]
fn one_byte_over_the_cap_is_cut_to_the_cap_with_the_marker() {
    let cap = 1024;
    let output = "x".repeat(cap + 1);
    let capped = cap_output(output.clone(), cap);

    assert!(capped.len() <= cap, "{} > {cap}", capped.len());
    let kept = stated_kept(&capped);
    assert_eq!(
        capped,
        format!(
            "{}{}",
            &output[..kept],
            truncation_marker(kept, cap + 1, cap)
        ),
        "the kept prefix, then the marker, and nothing else"
    );
    // ASCII loses nothing to a character boundary: the kept prefix is
    // the whole budget the marker's widest form leaves.
    assert_eq!(kept, cap - reserve(cap + 1, cap));
}

/// The wording is the contract with the model: what the cap is, the
/// exact counts, and that the rest is gone — with no suggestion that it
/// can be asked for, because no tool can return it yet (#947, #948).
#[test]
fn the_marker_states_the_cap_and_exact_counts_and_offers_nothing_more() {
    let capped = cap_output("a".repeat(10_000), 4096);
    let marker = &capped[capped.find("\n\n(").expect("marker")..];
    let kept = stated_kept(&capped);
    assert_eq!(
        marker,
        format!(
            "\n\n(truncated at the 4.0 KiB tool-result cap: kept the first {kept} of 10000 bytes; \
             the rest was dropped)"
        )
    );
    assert_eq!(capped.len(), kept + marker.len());
}

/// A cut that lands inside a multi-byte character moves back to the
/// character's start: the result is valid UTF-8 (a `String` could not
/// be anything else — the point is that building it does not panic) and
/// still no larger than the cap.
#[test]
fn multi_byte_text_straddling_the_cap_is_cut_on_a_character_boundary() {
    let cap = 1024;
    // A three-byte character repeated, so most cut points fall inside one.
    let output = "€".repeat(cap);
    let budget = cap - reserve(output.len(), cap);
    assert!(
        !output.is_char_boundary(budget),
        "the fixture must put the raw cut inside a character"
    );

    let capped = cap_output(output.clone(), cap);
    assert!(capped.len() <= cap);
    assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
    let kept = stated_kept(&capped);
    assert!(output.is_char_boundary(kept));
    assert!(capped.starts_with(&output[..kept]));
    assert!(kept < budget && budget - kept < '€'.len_utf8());
}

#[test]
fn a_cap_too_small_for_the_marker_keeps_the_start_of_the_marker() {
    for cap in [0, 1, 10, 40] {
        let capped = cap_output("y".repeat(500), cap);
        assert!(capped.len() <= cap, "cap {cap}: {capped:?}");
        assert!(
            truncation_marker(0, 500, cap).starts_with(&capped),
            "cap {cap}: {capped:?}"
        );
    }
}

#[test]
fn human_bytes_formats_sizes() {
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(2048), "2.0 KiB");
    assert_eq!(human_bytes(256 * 1024), "256.0 KiB");
    assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
}

proptest! {
    // Bounded by count, and each case by its input sizes.
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any text and any cap: never larger than the cap; untouched
    /// when it fits; otherwise a character-boundary prefix of the input
    /// followed by the marker, losing at most a character's worth of
    /// budget to the boundary.
    #[test]
    fn a_capped_result_never_exceeds_the_cap(output in "\\PC{0,600}", cap in 0usize..2400) {
        let capped = cap_output(output.clone(), cap);
        prop_assert!(capped.len() <= cap);
        if output.len() <= cap {
            prop_assert_eq!(capped, output);
        } else if cap >= reserve(output.len(), cap) {
            let kept = stated_kept(&capped);
            prop_assert!(output.is_char_boundary(kept));
            prop_assert_eq!(
                &capped,
                &format!("{}{}", &output[..kept], truncation_marker(kept, output.len(), cap))
            );
            let budget = cap - reserve(output.len(), cap);
            prop_assert!(kept <= budget && budget - kept < 4);
        }
    }
}
