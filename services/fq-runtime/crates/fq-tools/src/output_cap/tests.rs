use proptest::prelude::*;

use super::*;

/// The oracle: the bytes `text` takes inside a JSON string, read off
/// `serde_json` itself rather than off the helper under test.
fn wire_len(text: &str) -> usize {
    serde_json::to_string(text)
        .expect("a string serialises")
        .len()
        - 2
}

/// The room [`cap_output`] keeps for the marker: its widest form, for an
/// output of `total` bytes under `max_bytes`.
fn reserve(total: usize, max_bytes: usize) -> usize {
    wire_len(&truncation_marker(total, total, max_bytes))
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

/// Every byte JSON escapes, the characters around them, and multi-byte
/// text.
fn every_awkward_character() -> String {
    let mut text: String = (0u8..0x20).map(char::from).collect();
    text.push_str("\"\\/\u{7f} plain é € 😀 \u{2028}");
    text
}

#[test]
fn the_encoded_length_is_what_serde_json_writes() {
    let awkward = every_awkward_character();
    for text in [
        String::new(),
        "plain ascii".to_string(),
        awkward.clone(),
        awkward.repeat(7),
        "\u{0}".repeat(100),
        "\"\\".repeat(50),
    ] {
        assert_eq!(json_encoded_len(&text), wire_len(&text), "{text:?}");
    }
}

#[test]
fn output_that_fits_is_untouched() {
    let cap = 1024;
    for len in [0, 1, cap - 1, cap] {
        let output = "x".repeat(len);
        assert_eq!(cap_output(output.clone(), cap), output, "len {len}");
    }
    // Exactly the cap once encoded: each newline takes two bytes.
    let newlines = "\n".repeat(cap / 2);
    assert_eq!(wire_len(&newlines), cap);
    assert_eq!(cap_output(newlines.clone(), cap), newlines);
}

#[test]
fn one_byte_over_the_cap_is_cut_to_the_cap_with_the_marker() {
    let cap = 1024;
    let output = "x".repeat(cap + 1);
    let capped = cap_output(output.clone(), cap);

    assert!(wire_len(&capped) <= cap, "{} > {cap}", wire_len(&capped));
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
    // Plain ASCII takes one byte each and loses nothing to a character
    // boundary: the kept prefix is the whole budget the marker's widest
    // form leaves.
    assert_eq!(kept, cap - reserve(cap + 1, cap));
}

/// Text inside the cap in bytes can be outside it in the event: a quote
/// is written as two bytes. The cut is by the second number.
#[test]
fn text_under_the_cap_in_bytes_but_over_it_once_encoded_is_cut() {
    let cap = 4096;
    let output = "\"".repeat(3000);
    assert!(output.len() < cap && wire_len(&output) > cap);

    let capped = cap_output(output.clone(), cap);
    assert!(wire_len(&capped) <= cap, "{} > {cap}", wire_len(&capped));
    let kept = stated_kept(&capped);
    assert_eq!(kept, (cap - reserve(output.len(), cap)) / 2);
    assert!(capped.starts_with(&output[..kept]));
    assert!(capped.ends_with("of 3000 bytes; the rest was dropped)"));
}

/// The case that made the cap count encoded bytes: NUL is valid UTF-8,
/// so `file_read` returns a file full of it, and each one is six bytes
/// in the event. Counted as text, 256 KiB of it was 1.5 MiB published.
#[test]
fn nul_dense_text_is_cut_to_the_cap_as_the_event_carries_it() {
    let cap = 256 * 1024;
    let output = "\u{0}".repeat(300 * 1024);
    let capped = cap_output(output.clone(), cap);

    assert!(wire_len(&capped) <= cap, "{} > {cap}", wire_len(&capped));
    let kept = stated_kept(&capped);
    assert_eq!(kept, (cap - reserve(output.len(), cap)) / 6);
    assert!(capped.starts_with(&output[..kept]));
}

/// `String::truncate` keeps the allocation. A capped answer to a large
/// read must not go on holding the whole read.
#[test]
fn a_capped_result_lets_go_of_the_rest_of_its_allocation() {
    let cap = 4096;
    let capped = cap_output("x".repeat(1024 * 1024), cap);
    assert!(
        capped.capacity() < 2 * cap,
        "a {}-byte result still holds {} bytes",
        capped.len(),
        capped.capacity()
    );
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
/// character's start: building the result does not panic, and it is
/// still no larger than the cap.
#[test]
fn multi_byte_text_straddling_the_cap_is_cut_on_a_character_boundary() {
    let cap = 1024;
    // A three-byte character repeated, padded by 0, 1 or 2 bytes: one of
    // the three puts the raw cut (the budget the marker leaves, the same
    // for all three) inside a character. Pick that one.
    let (output, budget) = (0..3)
        .map(|pad| format!("{}{}", "a".repeat(pad), "€".repeat(cap)))
        .find_map(|output| {
            let budget = cap - reserve(output.len(), cap);
            (!output.is_char_boundary(budget)).then_some((output, budget))
        })
        .expect("one of three paddings cuts inside a character");

    let capped = cap_output(output.clone(), cap);
    assert!(wire_len(&capped) <= cap);
    let kept = stated_kept(&capped);
    assert!(output.is_char_boundary(kept));
    assert!(capped.starts_with(&output[..kept]));
    assert!(kept < budget && budget - kept < '€'.len_utf8());
}

#[test]
fn a_cap_too_small_for_the_marker_keeps_as_much_of_the_marker_as_fits() {
    for cap in [0, 1, 2, 3, 10, 40] {
        let capped = cap_output("y".repeat(500), cap);
        assert!(wire_len(&capped) <= cap, "cap {cap}: {capped:?}");
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

/// Characters weighted towards the ones JSON escapes, so most cases
/// carry several of them.
fn awkward_char() -> impl Strategy<Value = char> {
    prop_oneof![
        (0u8..0x20).prop_map(char::from),
        prop::sample::select(vec!['"', '\\', '/', '\u{7f}', 'é', '€', '😀']),
        prop::char::range('a', 'z'),
        any::<char>(),
    ]
}

fn awkward_text() -> impl Strategy<Value = String> {
    prop::collection::vec(awkward_char(), 0..400).prop_map(String::from_iter)
}

proptest! {
    // Bounded by count, and each case by its input sizes (at most 400
    // characters, so at most 1,600 bytes).
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn the_encoded_length_matches_serde_json(text in awkward_text()) {
        prop_assert_eq!(json_encoded_len(&text), wire_len(&text));
    }

    /// For any text and any cap: the result never takes more than the
    /// cap in an event; it is the input when the input fits; otherwise it
    /// is a prefix of the input ending on a character boundary, then the
    /// marker stating that prefix's length — the longest such prefix the
    /// marker's widest form leaves room for.
    #[test]
    fn a_capped_result_never_exceeds_the_cap_once_encoded(
        output in awkward_text(),
        cap in 0usize..2400,
    ) {
        let capped = cap_output(output.clone(), cap);
        prop_assert!(wire_len(&capped) <= cap);
        if wire_len(&output) <= cap {
            prop_assert_eq!(capped, output);
        } else if cap >= reserve(output.len(), cap) {
            let kept = stated_kept(&capped);
            prop_assert!(output.is_char_boundary(kept));
            prop_assert_eq!(
                &capped,
                &format!("{}{}", &output[..kept], truncation_marker(kept, output.len(), cap))
            );
            let budget = cap - reserve(output.len(), cap);
            prop_assert!(wire_len(&output[..kept]) <= budget);
            let next = output[kept..].chars().next().map_or(kept, |c| kept + c.len_utf8());
            prop_assert!(next == kept || wire_len(&output[..next]) > budget);
        } else {
            prop_assert!(truncation_marker(0, output.len(), cap).starts_with(&capped));
        }
    }
}
