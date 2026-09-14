//! The periodic pricing refresh (<https://github.com/bricef/factor-q/issues/344>):
//! the same acceptance the startup load runs, on a schedule, swapped
//! into a daemon that is already serving traffic.
//!
//! Pricing was fetched once at start and never again, so a daemon that
//! runs for weeks serves prices from the day it booted. fq-cron publishes
//! `fq.maintenance.pricing_refresh` on a cadence and the daemon's
//! maintenance consumer runs [`PricingRefresh::run`]; everything about
//! *what to accept* is [`accept`](super::accept) and [`live`],
//! unchanged. What is new is the two rules that make a swap safe under a
//! daemon that is already running.
//!
//! ## Widen-only, and where removals land
//!
//! **A refresh never takes a price away.** New models and accepted price
//! changes land immediately — that is the whole point — but a model the
//! served table prices keeps a price whatever the new document says.
//! Under [ADR-0004] an unpriced model is a *refused dispatch*
//! ([`enforce_pricing`](crate::RunnerConfig)), so a table that narrowed
//! under an invocation would brick it mid-flight, which is the #120/#276
//! failure family arriving by a new road. Widening cannot: a model that
//! gains a price, or whose price moves within the bound, changes what a
//! run costs and never whether it may run.
//!
//! **Removals are applied at daemon start**, and the startup load
//! applies them for free: [`accept`](super::accept::accept) does not
//! carry over a model the candidate has stopped listing, and the cache
//! this refresh writes is the accepted document, so a model upstream
//! dropped is already absent from the table the next start compares
//! against and from the one it serves. A refresh therefore holds a
//! retired price for at most one daemon lifetime, and the daemon does
//! not accumulate them across restarts.
//!
//! *The alternative was "the next refresh at which the model is
//! unreferenced", and it was rejected because nothing in this daemon can
//! answer "unreferenced" truthfully.* The nearest thing is the provider
//! throttle's per-model permit count, which counts LLM calls in flight
//! and reads zero for an invocation between two turns — it is running a
//! tool, and its next turn needs the price. The projection knows which
//! invocations are open but is derived and lags the events it folds. Both
//! would answer "unreferenced" for work that is very much in flight, and
//! being wrong in that direction is exactly the failure the rule exists
//! to prevent. Daemon start is the one boundary at which "nothing
//! references it" is a fact rather than an estimate, and it is also where
//! ADR-0004's coverage guarantee is enforced with an operator in front of
//! it: a start that would leave a declared model unpriced refuses to run
//! and names the model. Making "unreferenced" observable means a
//! per-invocation model lease through the reducer's lifecycle, which is
//! its own change and its own issue.
//!
//! ## Configuration wins
//!
//! The table the daemon serves is the accepted LiteLLM table with prices
//! layered over it: OpenRouter's catalogue for the models routed there,
//! and `[providers.<name>.pricing]` overrides. Those are **configuration**
//! — an operator's answer to a model the source prices wrongly or not at
//! all — so a refresh re-applies them last and upstream never displaces
//! one. They travel as a [`PricingOverlay`], captured once where the
//! daemon builds them; they change when the daemon restarts, as the
//! operating guide says they do.
//!
//! [ADR-0004]: ../../../../../docs/adrs/accepted/0004-cost-controls-from-day-one.md

use std::collections::BTreeMap;
use std::path::PathBuf;

use tracing::{info, warn};

use crate::events::PendingSignal;

use super::accept::Refusal;
use super::episodes::PricingEpisodes;
use super::live::{self, AcceptedLoad, LoadSettings};
use super::served::ServedPricing;
use super::{ModelPricing, PricingTable};

/// The prices that are not the accepted LiteLLM table's.
///
/// One value rather than "the daemon re-does its layering after every
/// refresh": the layering reaches an HTTP catalogue and the whole
/// `[providers]` section, and re-running it on a cadence would make a
/// refresh depend on a second network fetch and on config the daemon
/// has not re-read. What the refresh actually needs is the *result* —
/// which model ids the base table does not get to price, and what they
/// cost — and that is what this holds.
#[derive(Debug, Clone, Default)]
pub struct PricingOverlay {
    entries: BTreeMap<String, ModelPricing>,
}

impl PricingOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `model` is priced by configuration rather than by the
    /// LiteLLM table, at `pricing`.
    pub fn set(&mut self, model: impl Into<String>, pricing: ModelPricing) {
        self.entries.insert(model.into(), pricing);
    }

    /// Lay every overlay price over `table`, replacing whatever it held.
    pub fn apply(&self, table: &mut PricingTable) {
        for (model, pricing) in &self.entries {
            table.entries.insert(model.clone(), *pricing);
        }
    }

    /// Whether this model's price comes from configuration.
    pub fn covers(&self, model: &str) -> bool {
        self.entries.contains_key(model)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What one refresh did, in the terms the outcome event reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefreshReport {
    /// Models the table prices after the swap.
    pub entries: usize,
    /// Models this refresh priced that had no price before.
    pub added: usize,
    /// Models whose price this refresh moved.
    pub changed: usize,
    /// Changes acceptance refused — one per model, each also an
    /// operator signal.
    ///
    /// Refused *changes* only. A model the floor would not admit in the
    /// first place is not a refused change and is counted separately;
    /// see [`not_admitted`](Self::not_admitted).
    pub refused: usize,
    /// Models the plausibility floor would not admit — the live table's
    /// several hundred free, local and embedding entries priced at zero.
    ///
    /// Counted apart from [`refused`](Self::refused) because they are
    /// different news. A refused change is one model that moved
    /// implausibly and raises a notification an operator can act on; a
    /// not-admitted model raises nothing and is a standing property of
    /// the source, identical on every load. Folding the ~336 of them
    /// into `refused` made the outcome line read `336 refused` while
    /// zero signals were published (review D-3).
    pub not_admitted: usize,
    /// Models the served table prices that the accepted document no
    /// longer lists. They keep their price until the daemon restarts;
    /// see the module header.
    pub held: Vec<String>,
    /// The document did not land, so this refresh served — and re-served
    /// — the last accepted table.
    pub fetch_failed: bool,
}

impl RefreshReport {
    /// The one line the `maintenance_run` outcome carries.
    pub fn detail(&self) -> String {
        if self.fetch_failed {
            return format!(
                "fetch failed; still serving the last accepted table ({} entries)",
                self.entries
            );
        }
        let mut detail = format!(
            "{} entries ({} new, {} repriced, {} refused, {} not admitted)",
            self.entries, self.added, self.changed, self.refused, self.not_admitted
        );
        if !self.held.is_empty() {
            detail.push_str(&format!(
                "; {} no longer listed upstream, priced until restart",
                self.held.len()
            ));
        }
        detail
    }
}

/// A refresh that ran, and everything an operator should be told about
/// it.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub report: RefreshReport,
    /// What this refresh wants an operator told: a refused change per
    /// model, and the *edges* of the two standing conditions — a failed
    /// fetch or a stale table raised once per episode, and a
    /// notification resolving each when the next document lands (see
    /// [`PricingEpisodes`]).
    pub signals: Vec<PendingSignal>,
}

/// A refresh that did not happen. Not a fetch that failed — that is a
/// working state with a notification, and the report says so.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    /// The merge produced a table that prices fewer models than the one
    /// being served. Unreachable by construction — the merge starts from
    /// the served table — so this is a bug guard, and it fires *before*
    /// the swap: the daemon keeps serving a table it knows covers every
    /// declared model rather than one that might not.
    #[error(
        "refusing to swap in a pricing table that would unprice {}: {}",
        .models.len(),
        .models.join(", ")
    )]
    WouldNarrow { models: Vec<String> },
}

/// Everything a scheduled pricing refresh needs, assembled where the
/// daemon builds its price list and handed to the maintenance consumer.
#[derive(Debug, Clone)]
pub struct PricingRefresh {
    settings: LoadSettings,
    cache_path: PathBuf,
    overlay: PricingOverlay,
    served: ServedPricing,
    /// Which standing conditions have already been reported, shared with
    /// the startup load so a table found stale at boot and still stale
    /// at this refresh is one episode rather than two.
    episodes: PricingEpisodes,
    /// Where the document is fetched from — derived from
    /// `settings.source` at construction, and pointed elsewhere only by
    /// the test-only seam below.
    upstream: String,
}

impl PricingRefresh {
    /// `settings` and `cache_path` are the startup load's, so a refresh
    /// reads and writes the same accepted table the daemon booted from.
    pub fn new(
        settings: LoadSettings,
        cache_path: impl Into<PathBuf>,
        overlay: PricingOverlay,
        served: ServedPricing,
        episodes: PricingEpisodes,
    ) -> Self {
        let upstream = settings.source.url();
        Self {
            settings,
            cache_path: cache_path.into(),
            overlay,
            served,
            episodes,
            upstream,
        }
    }

    /// Test-only: fetch from `url` rather than from the configured
    /// source's own, so a test can drive a mock LiteLLM document through
    /// the whole path — fetch, accept, cache, merge, swap, signals —
    /// rather than asserting on the halves separately. Nothing in
    /// production reaches it, and `[pricing] source` grows no spelling
    /// for "some other URL".
    #[cfg(test)]
    pub(crate) fn with_upstream(mut self, url: impl Into<String>) -> Self {
        self.upstream = url.into();
        self
    }

    /// The table this refresh swaps.
    pub fn served(&self) -> &ServedPricing {
        &self.served
    }

    /// Fetch, accept, merge widen-only, swap, and say what happened.
    ///
    /// The prior the fetched document is judged against is the **last
    /// accepted table on disk**, not the table in memory. They are the
    /// same table minus the configuration layered over it, and bounding
    /// an operator's override against upstream's previous figure would
    /// refuse the very change the operator wrote the override to make.
    /// The cache is therefore load-bearing for the drift bound in the
    /// same way it is at startup: a cache that was deleted under a
    /// running daemon makes the next document's models all *new*, which
    /// the plausibility floor judges and the bound does not. Each
    /// refresh rewrites it, so the window is one cadence wide.
    pub async fn run(&self) -> Result<RefreshOutcome, RefreshError> {
        let load =
            live::load_accepted_from(&self.upstream, self.settings.clone(), &self.cache_path).await;
        self.settle(load)
    }

    /// Everything after the network: the merge, the guard, the swap.
    /// Separated so the rules are testable without a socket.
    fn settle(&self, load: AcceptedLoad) -> Result<RefreshOutcome, RefreshError> {
        let signals = self.episodes.edges(&load, load.signals());
        let current = self.served.current();
        let (next, mut report) = merge(&current, &load.table, &self.overlay);
        let (not_admitted, refused): (Vec<&Refusal>, Vec<&Refusal>) =
            load.refusals.iter().partition(|r| r.is_admission());
        report.refused = refused.len();
        report.not_admitted = not_admitted.len();
        report.fetch_failed = load.fetch_error.is_some();

        let unpriced: Vec<String> = current
            .entries
            .keys()
            .filter(|model| !next.entries.contains_key(*model))
            .cloned()
            .collect();
        if !unpriced.is_empty() {
            return Err(RefreshError::WouldNarrow { models: unpriced });
        }

        self.served.swap(next);
        if report.held.is_empty() {
            info!(
                entries = report.entries,
                added = report.added,
                repriced = report.changed,
                refused = report.refused,
                not_admitted = report.not_admitted,
                "swapped in a refreshed pricing table"
            );
        } else {
            // At `warn` because it is the one state an operator may want
            // to act on: a model factor-q still prices and upstream has
            // stopped publishing. The names are here rather than in a
            // notification per model — the set is identical on every
            // refresh until a restart clears it, so it is a property of
            // the table rather than something that happened.
            warn!(
                entries = report.entries,
                held = ?report.held,
                "swapped in a refreshed pricing table; these models are no longer listed \
                 upstream and keep their last accepted price until this daemon restarts"
            );
        }
        Ok(RefreshOutcome { report, signals })
    }
}

/// Fold an accepted LiteLLM table into the table being served, without
/// ever taking a price away.
///
/// The result starts as the served table — which is what makes the merge
/// widen-only — then takes every price and window the accepted table
/// offers, then the overlay, which is configuration and wins. The
/// provenance is the accepted table's: it names the bytes the prices
/// came from, and every cost row written after the swap cites it.
fn merge(
    current: &PricingTable,
    accepted: &PricingTable,
    overlay: &PricingOverlay,
) -> (PricingTable, RefreshReport) {
    let mut next = current.clone();
    let mut added = 0usize;
    let mut changed = 0usize;
    for (model, pricing) in &accepted.entries {
        if overlay.covers(model) {
            // Applied below from the overlay; counting it as a change
            // here would report a reprice that never reaches the table.
            continue;
        }
        match next.entries.insert(model.clone(), *pricing) {
            None => added += 1,
            Some(previous) if moved(&previous, pricing) => changed += 1,
            Some(_) => {}
        }
    }
    for (model, window) in &accepted.context_windows {
        next.context_windows.insert(model.clone(), *window);
    }
    overlay.apply(&mut next);
    next.provenance = accepted.provenance.clone();

    // What upstream has stopped listing, ignoring everything upstream
    // never priced under that id in the first place: the overlay's
    // models, and the gateway-routed ids the daemon prices from a
    // differently-spelled LiteLLM key.
    let held: Vec<String> = current
        .entries
        .keys()
        .filter(|model| !accepted.entries.contains_key(*model) && !overlay.covers(model))
        .cloned()
        .collect();

    let report = RefreshReport {
        entries: next.entries.len(),
        added,
        changed,
        refused: 0,
        not_admitted: 0,
        held,
        fetch_failed: false,
    };
    (next, report)
}

/// Whether a model's price actually moved. Compared field by field on
/// the stored per-million figures, so a document that restates the same
/// numbers reports no change — which is what makes `repriced` in the
/// outcome mean something on a daemon that refreshes every six hours.
fn moved(previous: &ModelPricing, proposed: &ModelPricing) -> bool {
    previous.input_per_million != proposed.input_per_million
        || previous.output_per_million != proposed.output_per_million
        || previous.cache_read_per_million != proposed.cache_read_per_million
        || previous.cache_write_per_million != proposed.cache_write_per_million
}

/// The models a refresh must leave priced: everything the served table
/// priced before it. Used by the property test, and the statement of the
/// rule this module exists to keep.
#[cfg(test)]
pub(crate) fn priced_models(table: &PricingTable) -> std::collections::BTreeSet<String> {
    table.entries.keys().cloned().collect()
}

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;
