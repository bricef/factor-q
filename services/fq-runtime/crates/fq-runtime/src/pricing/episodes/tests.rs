use chrono::Utc;
use std::time::Duration;

use super::*;
use crate::events::SignalSeverity;
use crate::pricing::PricingTable;
use crate::pricing::accept::{Disposition, PriceField, Refusal, RefusalRule};
use crate::pricing::live::Staleness;

/// A load that could not fetch, with a table `days` past its window.
fn stale_load(days: i64) -> AcceptedLoad {
    AcceptedLoad {
        table: PricingTable::empty(),
        refusals: Vec::new(),
        staleness: Some(Staleness {
            accepted_at: Utc::now() - chrono::Duration::days(days),
            age: Duration::from_secs(days as u64 * 24 * 3_600),
            max_age: Duration::from_secs(7 * 24 * 3_600),
        }),
        fetch_error: Some("connection refused".to_string()),
    }
}

/// A load that accepted a document: nothing failed and nothing is old.
fn accepted_load() -> AcceptedLoad {
    AcceptedLoad {
        table: PricingTable::empty(),
        refusals: Vec::new(),
        staleness: None,
        fetch_error: None,
    }
}

fn refused_load(refusals: &[(&str, PriceField)]) -> AcceptedLoad {
    AcceptedLoad {
        table: PricingTable::empty(),
        refusals: refusals
            .iter()
            .map(|(model, field)| Refusal {
                model: (*model).to_string(),
                field: *field,
                old: Some(1e-6),
                new: 6e-6,
                ratio: Some(6.0),
                rule: RefusalRule::DriftBound,
                disposition: Disposition::KeptPriorPrice,
            })
            .collect(),
        staleness: None,
        fetch_error: None,
    }
}

fn kinds_of(signals: &[PendingSignal]) -> Vec<&str> {
    signals.iter().map(|s| s.kind().as_str()).collect()
}

#[test]
fn a_repeated_refusal_is_raised_once_then_resolved() {
    let episodes = PricingEpisodes::new();
    let refused = refused_load(&[("a/one", PriceField::Input)]);
    let raised = episodes.edges(&refused, refused.signals());
    assert_eq!(kinds_of(&raised), vec!["pricing.change_refused"]);

    let repeated = refused_load(&[("a/one", PriceField::Input)]);
    assert!(episodes.edges(&repeated, repeated.signals()).is_empty());

    let recovered = episodes.edges(&accepted_load(), accepted_load().signals());
    assert_eq!(kinds_of(&recovered), vec!["pricing.change_refused"]);
    assert_eq!(recovered[0].payload.resolves, Some(raised[0].event_id));
    assert!(
        recovered[0]
            .payload
            .summary
            .contains("a/one.input_cost_per_token")
    );
}

#[test]
fn different_refusal_keys_raise_independently() {
    let episodes = PricingEpisodes::new();
    let refused = refused_load(&[("a/one", PriceField::Input), ("a/one", PriceField::Output)]);
    let raised = episodes.edges(&refused, refused.signals());
    assert_eq!(kinds_of(&raised), vec!["pricing.change_refused"; 2]);
    assert_ne!(raised[0].event_id, raised[1].event_id);
}

#[test]
fn a_refusal_that_reappears_gets_a_new_id() {
    let episodes = PricingEpisodes::new();
    let refused = refused_load(&[("a/one", PriceField::Input)]);
    let first = episodes.edges(&refused, refused.signals());
    let resolved = episodes.edges(&accepted_load(), accepted_load().signals());
    let refused_again = refused_load(&[("a/one", PriceField::Input)]);
    let second = episodes.edges(&refused_again, refused_again.signals());

    assert_eq!(resolved[0].payload.resolves, Some(first[0].event_id));
    assert_ne!(first[0].event_id, second[0].event_id);
    assert!(second[0].payload.resolves.is_none());
}

/// Three refreshes while the condition holds produce one alert. Without
/// this, a week of a broken upstream is twenty-eight open alerts that
/// nothing can ever close.
#[test]
fn a_condition_that_holds_is_raised_once() {
    let episodes = PricingEpisodes::new();

    let first = episodes.edges(&stale_load(9), stale_load(9).signals());
    assert_eq!(
        kinds_of(&first),
        vec!["pricing.fetch_failed", "pricing.stale"],
        "the first refresh of an episode says both"
    );

    for refresh in 2..=3 {
        let again = episodes.edges(&stale_load(9 + refresh), stale_load(9 + refresh).signals());
        assert!(
            again.is_empty(),
            "refresh {refresh} repeated a condition already reported: {:?}",
            kinds_of(&again)
        );
    }
}

/// The recovery names the alert it closes, carries its kind, and is a
/// notification — recovering is not itself alarming.
#[test]
fn a_document_that_lands_resolves_what_it_ended() {
    let episodes = PricingEpisodes::new();
    let raised = episodes.edges(&stale_load(9), stale_load(9).signals());
    let stale = raised
        .iter()
        .find(|s| s.kind().as_str() == "pricing.stale")
        .expect("the alert was raised");
    let failed = raised
        .iter()
        .find(|s| s.kind().as_str() == "pricing.fetch_failed")
        .expect("the fetch failure was raised");
    assert_eq!(stale.payload.severity, SignalSeverity::Alert);

    let recovered = episodes.edges(&accepted_load(), accepted_load().signals());

    assert_eq!(
        kinds_of(&recovered),
        vec!["pricing.stale", "pricing.fetch_failed"],
        "one recovery per condition that ended"
    );
    assert_eq!(
        recovered[0].payload.resolves,
        Some(stale.event_id),
        "the recovery names the alert it closes"
    );
    assert_eq!(
        recovered[0].payload.severity,
        SignalSeverity::Notification,
        "a recovery is not itself alarming"
    );
    assert_eq!(recovered[1].payload.resolves, Some(failed.event_id));
}

/// An episode that ended and began again is two alerts, not one — the
/// second is a new thing to act on and the first has been answered.
#[test]
fn the_next_episode_raises_a_new_alert() {
    let episodes = PricingEpisodes::new();
    let first = episodes.edges(&stale_load(9), stale_load(9).signals());
    episodes.edges(&accepted_load(), accepted_load().signals());

    let second = episodes.edges(&stale_load(20), stale_load(20).signals());

    assert_eq!(
        kinds_of(&second),
        vec!["pricing.fetch_failed", "pricing.stale"]
    );
    let first_alert = first
        .iter()
        .find(|s| s.kind().as_str() == "pricing.stale")
        .expect("raised");
    let second_alert = second
        .iter()
        .find(|s| s.kind().as_str() == "pricing.stale")
        .expect("raised again");
    assert_ne!(
        first_alert.event_id, second_alert.event_id,
        "a new episode is a new alert"
    );
    assert!(
        second_alert.payload.resolves.is_none(),
        "a raise resolves nothing"
    );
}

/// Nothing to resolve is nothing to say: a run of successful refreshes
/// does not emit recoveries for conditions that were never raised.
#[test]
fn a_run_that_was_never_broken_says_nothing() {
    let episodes = PricingEpisodes::new();
    for _ in 0..3 {
        assert!(
            episodes
                .edges(&accepted_load(), accepted_load().signals())
                .is_empty()
        );
    }
}

/// A fetch that fails while the table is still inside its window is one
/// notification, and the staleness alert that follows later is its own
/// episode rather than a recurrence of the notification.
#[test]
fn a_failed_fetch_inside_the_window_does_not_open_the_stale_episode() {
    let episodes = PricingEpisodes::new();
    let fresh_but_failed = AcceptedLoad {
        table: PricingTable::empty(),
        refusals: Vec::new(),
        staleness: None,
        fetch_error: Some("connection refused".to_string()),
    };

    let first = episodes.edges(&fresh_but_failed, fresh_but_failed.signals());
    assert_eq!(kinds_of(&first), vec!["pricing.fetch_failed"]);

    // Later, still failing, and now past the window.
    let now_stale = episodes.edges(&stale_load(9), stale_load(9).signals());
    assert_eq!(
        kinds_of(&now_stale),
        vec!["pricing.stale"],
        "the fetch failure is not re-raised; the staleness is new"
    );
}
