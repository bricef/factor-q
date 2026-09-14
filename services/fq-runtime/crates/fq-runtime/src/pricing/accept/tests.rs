use serde_json::json;

use super::*;

fn priced(input: f64, output: f64) -> ModelPricing {
    ModelPricing {
        input_per_million: input,
        output_per_million: output,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }
}

fn table(entries: &[(&str, ModelPricing)]) -> PricingTable {
    let mut table = PricingTable::empty();
    for (model, pricing) in entries {
        table.insert(*model, *pricing);
    }
    table
}

fn input_of(table: &PricingTable, model: &str) -> f64 {
    table
        .lookup(model)
        .expect("model is priced")
        .input_per_million
}

/// #735 acceptance box 1: a 6x move is refused for that model, every
/// other change lands, and one refusal names model, field, old, new and
/// ratio.
#[test]
fn a_six_times_move_keeps_the_prior_price_and_records_one_refusal() {
    let prior = table(&[
        ("moonshotai/kimi-k3", priced(0.6, 2.5)),
        ("claude-haiku-4-5", priced(1.0, 5.0)),
    ]);
    let candidate = table(&[
        ("moonshotai/kimi-k3", priced(3.7, 2.5)),
        // A real repricing, well inside the bound: it lands.
        ("claude-haiku-4-5", priced(1.4, 7.0)),
    ]);

    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());

    assert_eq!(
        input_of(&accepted, "moonshotai/kimi-k3"),
        0.6,
        "the refused model keeps its prior price"
    );
    assert_eq!(
        input_of(&accepted, "claude-haiku-4-5"),
        1.4,
        "every other change is accepted"
    );
    assert_eq!(refusals.len(), 1, "one refusal, for one model");
    let refusal = &refusals[0];
    assert_eq!(refusal.model, "moonshotai/kimi-k3");
    assert_eq!(refusal.field, PriceField::Input);
    assert_eq!(refusal.rule, RefusalRule::DriftBound);
    // Per token, as the source states prices.
    assert!((refusal.old.unwrap() - 6e-7).abs() < 1e-18);
    assert!((refusal.new - 3.7e-6).abs() < 1e-18);
    assert!((refusal.ratio.unwrap() - 6.166).abs() < 0.01);
    assert!(!refusal.is_admission());
}

/// A move inside the bound is not news. The bound is a nonsense
/// detector, not a change detector.
#[test]
fn a_move_inside_the_bound_is_accepted_silently() {
    let prior = table(&[("m", priced(1.0, 5.0))]);
    let candidate = table(&[("m", priced(4.9, 24.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert_eq!(input_of(&accepted, "m"), 4.9);
    assert!(refusals.is_empty());
    // ... and so is the same move downward.
    let (accepted, refusals) = accept(
        &table(&[("m", priced(5.0, 25.0))]),
        table(&[("m", priced(1.0, 5.0))]),
        AcceptanceRules::default(),
    );
    assert_eq!(input_of(&accepted, "m"), 1.0);
    assert!(refusals.is_empty());
}

/// The bound is symmetric: a fall to a sixth is the same nonsense as a
/// rise of six.
#[test]
fn a_collapse_beyond_the_bound_is_refused_too() {
    let prior = table(&[("m", priced(6.0, 30.0))]);
    let candidate = table(&[("m", priced(0.9, 30.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert_eq!(input_of(&accepted, "m"), 6.0);
    assert_eq!(refusals[0].rule, RefusalRule::DriftBound);
    assert!(refusals[0].ratio.unwrap() > 5.0);
}

/// #735 acceptance box 2, first half: a table that zeroes a model is
/// loaded with the model at its prior price.
#[test]
fn zeroing_a_priced_model_is_refused_and_the_prior_price_stays() {
    let prior = table(&[("m", priced(1.0, 5.0))]);
    let candidate = table(&[("m", priced(0.0, 5.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert_eq!(input_of(&accepted, "m"), 1.0);
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].rule, RefusalRule::ZeroPrice);
    assert_eq!(refusals[0].field, PriceField::Input);
    assert_eq!(refusals[0].new, 0.0);
    assert_eq!(refusals[0].ratio, None, "there is no ratio to a zero");
}

/// #735 acceptance box 2, second half: a brand-new zero-priced model is
/// refused *at admission* — it never enters the table, which is what
/// leaves ADR-0004's at-use backstop to refuse the dispatch. The
/// backstop itself is exercised in the reducer runner's tests.
#[test]
fn a_new_model_priced_at_zero_is_not_admitted() {
    let prior = PricingTable::empty();
    let candidate = table(&[
        ("free/model", priced(0.0, 0.0)),
        ("real/model", priced(1.0, 5.0)),
    ]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert!(
        accepted.lookup("free/model").is_none(),
        "an unpriced model must be absent, not present at $0"
    );
    assert!(accepted.lookup("real/model").is_some());
    assert_eq!(refusals.len(), 1);
    assert!(refusals[0].is_admission());
    assert_eq!(refusals[0].old, None);
    assert_eq!(refusals[0].rule, RefusalRule::ZeroPrice);
}

/// The plausibility floor covers every category the model reports, not
/// just the two that are always there.
#[test]
fn a_new_model_with_a_zero_cache_rate_is_not_admitted() {
    let candidate = table(&[(
        "m",
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: Some(0.0),
            cache_write_per_million: None,
        },
    )]);
    let (accepted, refusals) = accept(
        &PricingTable::empty(),
        candidate,
        AcceptanceRules::default(),
    );
    assert!(accepted.lookup("m").is_none());
    assert_eq!(refusals[0].field, PriceField::CacheRead);
    assert_eq!(refusals[0].rule, RefusalRule::ZeroPrice);
}

/// A negative or non-finite price is the same nonsense as a zero
/// arriving by another route, and wears the same rule.
#[test]
fn a_negative_or_infinite_price_is_refused_as_a_zero() {
    let (accepted, refusals) = accept(
        &table(&[("m", priced(1.0, 5.0))]),
        table(&[("m", priced(-1.0, 5.0))]),
        AcceptanceRules::default(),
    );
    assert_eq!(input_of(&accepted, "m"), 1.0);
    assert_eq!(refusals[0].rule, RefusalRule::ZeroPrice);

    let (accepted, refusals) = accept(
        &PricingTable::empty(),
        table(&[("m", priced(f64::INFINITY, 5.0))]),
        AcceptanceRules::default(),
    );
    assert!(accepted.lookup("m").is_none());
    assert_eq!(refusals[0].rule, RefusalRule::ZeroPrice);
}

/// A model that was free and now costs money is **admitted at the new
/// price**. A zero prior is not a price to bound a move against, so the
/// model is judged as if it had no prior at all: plausibility alone.
///
/// This inverts the rule as it first shipped, which kept the $0 and
/// re-refused the change on every load — the daemon billing a model at
/// zero for ever after upstream said it costs money.
#[test]
fn a_zero_prior_price_moving_up_is_admitted_on_plausibility_alone() {
    let prior = table(&[("m", priced(0.0, 0.0))]);
    let candidate = table(&[("m", priced(1.0, 5.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert_eq!(input_of(&accepted, "m"), 1.0);
    assert!(
        refusals.is_empty(),
        "a plausible price over an implausible prior is an admission, not a refusal: {refusals:?}"
    );
}

/// The other half: a zero prior that is still zero is refused **at
/// admission** and dropped, exactly as it would have been on a clean
/// cache. Without this a table cached before acceptance existed — the
/// raw upstream document, several hundred free and embedding entries at
/// $0 — would launder every one of them into the accepted table.
#[test]
fn a_zero_prior_that_is_still_zero_is_dropped_rather_than_kept() {
    let prior = table(&[("free/model", priced(0.0, 0.0)), ("m", priced(1.0, 5.0))]);
    let candidate = table(&[("free/model", priced(0.0, 0.0)), ("m", priced(1.0, 5.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert!(
        accepted.lookup("free/model").is_none(),
        "a model priced at zero must be absent, not carried over at $0"
    );
    assert_eq!(input_of(&accepted, "m"), 1.0);
    assert_eq!(refusals.len(), 1);
    assert!(
        refusals[0].is_admission(),
        "the model is dropped, so the refusal has no prior price to name"
    );
    assert_eq!(refusals[0].old, None);
    assert_eq!(refusals[0].rule, RefusalRule::ZeroPrice);
}

/// A prior that fails the floor on *any* category it reports is not a
/// prior: the model is re-judged at admission, and the accepted document
/// drops it rather than splicing the old entry back in.
#[test]
fn an_implausible_prior_is_dropped_from_the_accepted_document() {
    let prior_doc = document(&[("free/model", entry(0.0)), ("m", entry(1e-6))]);
    let fetched_doc = document(&[("free/model", entry(0.0)), ("m", entry(1e-6))]);
    let prior = PricingTable::from_litellm_document(&prior_doc);
    let candidate = PricingTable::from_litellm_document(&fetched_doc);

    let (_, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    let doc = accepted_document(&prior_doc, fetched_doc, &refusals);
    assert!(
        !doc.contains_key("free/model"),
        "the cache must not hold what the table refused"
    );
    assert!(doc.contains_key("m"));
}

/// A priced model that newly publishes a *zero* cache rate is refused
/// and **reverts**: the refusal names a field the prior never priced, so
/// it carries no `old`, and what happened to the model is carried rather
/// than inferred from that absence.
#[test]
fn a_newly_published_zero_cache_rate_reverts_rather_than_dropping_the_model() {
    let prior = table(&[("m", priced(1.0, 5.0))]);
    let candidate = table(&[(
        "m",
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: Some(0.0),
            cache_write_per_million: None,
        },
    )]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());

    assert_eq!(input_of(&accepted, "m"), 1.0, "the model stays priced");
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].field, PriceField::CacheRead);
    assert_eq!(refusals[0].old, None, "the prior priced no cache read");
    assert!(
        !refusals[0].is_admission(),
        "a refused change reverts, whichever field failed"
    );
    assert_eq!(
        refusals[0].summary(),
        "m newly reports cache_read_input_token_cost = 0; kept the prior price"
    );
}

/// A model reverts whole. Half a model's prices from one document and
/// half from another is not a price list.
#[test]
fn a_refused_model_reverts_every_field_not_just_the_offending_one() {
    let prior = table(&[("m", priced(1.0, 5.0))]);
    let candidate = table(&[("m", priced(60.0, 6.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    let entry = accepted.lookup("m").unwrap();
    assert_eq!(entry.input_per_million, 1.0);
    assert_eq!(
        entry.output_per_million, 5.0,
        "the output price, inside the bound on its own, reverts with the model"
    );
    assert_eq!(refusals.len(), 1, "and it is still one refusal");
    assert_eq!(
        refusals[0].field,
        PriceField::Input,
        "named for the first field that failed"
    );
}

/// Dropping a published cache rate is not a refusable change: a model
/// without one is charged at the base input rate, which never
/// under-bills.
#[test]
fn losing_a_cache_rate_is_accepted() {
    let prior = table(&[(
        "m",
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_read_per_million: Some(0.1),
            cache_write_per_million: None,
        },
    )]);
    let candidate = table(&[("m", priced(1.0, 5.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert!(refusals.is_empty());
    assert_eq!(accepted.lookup("m").unwrap().cache_read_per_million, None);
}

/// The ratio is configuration: a deployment that wants a tighter bound
/// gets one.
#[test]
fn the_bound_is_the_configured_ratio() {
    let strict = AcceptanceRules {
        max_drift_ratio: 1.5,
    };
    let (accepted, refusals) = accept(
        &table(&[("m", priced(1.0, 5.0))]),
        table(&[("m", priced(2.0, 5.0))]),
        strict,
    );
    assert_eq!(input_of(&accepted, "m"), 1.0);
    assert_eq!(refusals.len(), 1);
}

/// A load is a safe boundary, so a model the source has stopped listing
/// is not carried over from the cache.
#[test]
fn a_model_the_candidate_no_longer_lists_is_not_carried_over() {
    let prior = table(&[("gone", priced(1.0, 5.0)), ("stays", priced(1.0, 5.0))]);
    let candidate = table(&[("stays", priced(1.0, 5.0))]);
    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert!(accepted.lookup("gone").is_none());
    assert!(accepted.lookup("stays").is_some());
    assert!(refusals.is_empty());
}

/// The refusal list is a function of the two tables, not of hash
/// iteration order — the notifications built from it must not reshuffle
/// between runs.
#[test]
fn refusals_come_out_in_model_order() {
    let prior = table(&[
        ("c", priced(1.0, 5.0)),
        ("a", priced(1.0, 5.0)),
        ("b", priced(1.0, 5.0)),
    ]);
    let candidate = table(&[
        ("c", priced(0.0, 5.0)),
        ("a", priced(0.0, 5.0)),
        ("b", priced(0.0, 5.0)),
    ]);
    let (_, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    let models: Vec<&str> = refusals.iter().map(|r| r.model.as_str()).collect();
    assert_eq!(models, ["a", "b", "c"]);
}

#[test]
fn a_refusal_summarises_itself_for_the_pane() {
    let prior = table(&[("moonshotai/kimi-k3", priced(0.6, 2.5))]);
    let candidate = table(&[("moonshotai/kimi-k3", priced(3.7, 2.5))]);
    let (_, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    assert_eq!(
        refusals[0].summary(),
        "moonshotai/kimi-k3 input_cost_per_token moved 6.2x; kept the prior price"
    );
}

// ---- the document splice -------------------------------------------

fn entry(input: f64) -> Value {
    json!({
        "input_cost_per_token": input,
        "output_cost_per_token": 5e-6,
        "litellm_provider": "somebody",
    })
}

fn document(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .map(|(model, value)| ((*model).to_string(), value.clone()))
        .collect()
}

#[test]
fn the_accepted_document_takes_a_refused_model_from_the_prior_one() {
    let prior = document(&[("m", entry(1e-6)), ("other", entry(2e-6))]);
    let fetched = document(&[("m", entry(6e-6)), ("other", entry(2e-6))]);
    let refusals = vec![Refusal {
        model: "m".to_string(),
        field: PriceField::Input,
        old: Some(1e-6),
        new: 6e-6,
        ratio: Some(6.0),
        rule: RefusalRule::DriftBound,
        disposition: Disposition::KeptPriorPrice,
    }];

    let accepted = accepted_document(&prior, fetched, &refusals);
    assert_eq!(accepted["m"], entry(1e-6));
    assert_eq!(accepted["other"], entry(2e-6));
}

#[test]
fn a_model_refused_at_admission_leaves_the_document() {
    let prior = document(&[]);
    let fetched = document(&[("new", entry(0.0)), ("other", entry(2e-6))]);
    let refusals = vec![Refusal {
        model: "new".to_string(),
        field: PriceField::Input,
        old: None,
        new: 0.0,
        ratio: None,
        rule: RefusalRule::ZeroPrice,
        disposition: Disposition::NotAdmitted,
    }];

    let accepted = accepted_document(&prior, fetched, &refusals);
    assert!(!accepted.contains_key("new"));
    assert_eq!(accepted.len(), 1);
}

/// The document is the unit that is cached and digested, so it must
/// carry upstream's own fields through untouched — including the ones
/// the table never parses.
#[test]
fn the_accepted_document_preserves_fields_the_table_does_not_read() {
    let fetched = document(&[("m", entry(1e-6))]);
    let accepted = accepted_document(&Map::new(), fetched, &[]);
    assert_eq!(accepted["m"]["litellm_provider"], json!("somebody"));
}

/// The table the loader serves and the document it caches have to agree:
/// re-parsing the spliced document must reproduce the accepted prices.
#[test]
fn the_accepted_document_reparses_to_the_accepted_table() {
    let prior_doc = document(&[("m", entry(1e-6))]);
    let fetched_doc = document(&[("m", entry(6e-6)), ("new", entry(3e-6))]);
    let prior = PricingTable::from_litellm_document(&prior_doc);
    let candidate = PricingTable::from_litellm_document(&fetched_doc);

    let (accepted, refusals) = accept(&prior, candidate, AcceptanceRules::default());
    let doc = accepted_document(&prior_doc, fetched_doc, &refusals);
    let reparsed = PricingTable::from_litellm_document(&doc);

    assert_eq!(reparsed.len(), accepted.len());
    for model in ["m", "new"] {
        assert_eq!(
            reparsed.lookup(model).unwrap().input_per_million,
            accepted.lookup(model).unwrap().input_per_million,
        );
    }
}
