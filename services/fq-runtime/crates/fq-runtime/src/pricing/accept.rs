//! What the daemon accepts from a live pricing table.
//!
//! The source is the live LiteLLM document, not a pinned commit, and the
//! discipline is on acceptance rather than on which commit is fetched
//! (<https://github.com/bricef/factor-q/issues/735>). Pinning turns
//! "prices drift" into "the daemon refuses to run new models until a
//! human bumps a SHA", and under ADR-0004's guarantee an unpriced model
//! is refused at startup — so a pin makes an automated process a manual
//! one on the schedule of somebody else's release notes. The threat was
//! never "upstream changed"; that is the point of the source. The threat
//! is upstream changing *badly*: a price zeroed by mistake or by
//! compromise, a nonsense multiplier, a malformed file.
//!
//! So [`accept`] stands between the fetched table and the running
//! daemon:
//!
//! 1. **Drift bound.** A price that moves by more than
//!    [`AcceptanceRules::max_drift_ratio`] in either direction is not
//!    accepted *for that model*: the prior price stays, the rest of the
//!    table lands, and the refusal is recorded.
//!    The bound is per load and measured against the last accepted price,
//!    so repeated in-bound moves are unbounded in aggregate. This deliberate
//!    trade-off guards against one bad upstream change rather than limiting
//!    where a price can eventually get to.
//! 2. **Zero refusal.** A model priced at zero where it was not is
//!    refused the same way. A *new* model priced at zero is refused at
//!    admission — never entering the table at all, which leaves
//!    ADR-0004's at-use backstop to refuse the dispatch rather than
//!    letting it run at $0.
//! 3. **Plausibility floor for new models.** A model with no prior is
//!    accepted only if every token category it reports carries a
//!    positive price. **A prior that fails this floor is not a prior:**
//!    a zero in the cache is no price to bound a move against, so the
//!    model is judged at admission instead. Still zero, and it is
//!    dropped; priced at last, and it is admitted on plausibility alone.
//!    No accepted table therefore holds a price of zero, whichever route
//!    the model took into it.
//!
//! **One refusal per model.** A model whose change is refused reverts
//! whole — a table cannot hold half a model's prices from one document
//! and half from another and still be a price list — so the refusal
//! names the first field that failed, in a fixed order, and the operator
//! signal that carries it is one notification per model per load.
//!
//! **Pure, so the refresh can call it too.** Nothing here fetches,
//! writes or publishes: it takes the last accepted table and a candidate
//! and returns what to serve and what to say. The startup path in
//! [`live`](super::live) and the periodic refresh
//! (<https://github.com/bricef/factor-q/issues/344>) are then the same
//! decision run at two times.

use serde_json::{Map, Value};

use super::{ModelPricing, PricingTable};

/// The bounds acceptance applies. Configuration, not code (Design
/// Principle 8) — see `[pricing]` in `fqd.toml`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcceptanceRules {
    /// How far a price may move, in either direction, and still be
    /// accepted. `5.0` means a fivefold rise or a fall to a fifth.
    ///
    /// Model prices move a lot and quickly, so the bound is loose on
    /// purpose: it is a nonsense detector, not a change detector.
    pub max_drift_ratio: f64,
}

/// The maintainer's bound: model prices move a lot and quickly, and 5×
/// is the margin that separates a real repricing from a mistake.
pub const DEFAULT_MAX_DRIFT_RATIO: f64 = 5.0;

impl Default for AcceptanceRules {
    fn default() -> Self {
        Self {
            max_drift_ratio: DEFAULT_MAX_DRIFT_RATIO,
        }
    }
}

/// Which of a model's prices a refusal is about, spelled as the source
/// spells it — the operator reading the notification is going to open
/// the upstream file and look for this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriceField {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    CacheWrite1h,
}

impl PriceField {
    /// The upstream field name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input_cost_per_token",
            Self::Output => "output_cost_per_token",
            Self::CacheRead => "cache_read_input_token_cost",
            Self::CacheWrite => "cache_creation_input_token_cost",
            Self::CacheWrite1h => "cache_creation_input_token_cost_above_1hr",
        }
    }

    /// The order fields are judged in, which is the order a refusal
    /// picks its field from. Fixed so the same pair of tables always
    /// produces the same refusal.
    pub const ORDER: [PriceField; 5] = [
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::CacheWrite1h,
    ];

    /// This field's price on a model, as the table stores it. `None`
    /// where the model reports no such price at all.
    ///
    /// Judging happens in these units and not in the source's, because
    /// the two differ by a factor of a million and a ratio taken after
    /// that division is not always the ratio taken before it: a price
    /// that moved by exactly the bound came out at 5.000000000000001x
    /// and was refused.
    fn per_million(self, pricing: &ModelPricing) -> Option<f64> {
        match self {
            Self::Input => Some(pricing.input_per_million),
            Self::Output => Some(pricing.output_per_million),
            Self::CacheRead => pricing.cache_read_per_million,
            Self::CacheWrite => pricing.cache_write_per_million,
            Self::CacheWrite1h => pricing.cache_write_1h_per_million,
        }
    }

    /// This field's price in the source's units, for a refusal to
    /// report: the operator reading it is going to open the upstream
    /// file, where prices are per token.
    fn per_token(self, pricing: &ModelPricing) -> Option<f64> {
        self.per_million(pricing).map(|p| p / 1_000_000.0)
    }
}

impl std::fmt::Display for PriceField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which rule refused a change. The two the operator-signal registry
/// documents for `pricing.change_refused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalRule {
    /// The price moved further than [`AcceptanceRules::max_drift_ratio`],
    /// or moved from zero (a move with no ratio at all).
    DriftBound,
    /// The price is not a positive number: zero, negative, or not
    /// finite. Covers both a change to zero and a new model admitted at
    /// zero.
    ZeroPrice,
}

impl RefusalRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DriftBound => "drift_bound",
            Self::ZeroPrice => "zero_price",
        }
    }
}

impl std::fmt::Display for RefusalRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a refusal did with the model — the fact every reader of a
/// refusal switches on, so it is carried rather than inferred.
///
/// It was inferred, from `old.is_none()`, and that is a different
/// question: a model whose prior reported no cache rate and whose
/// candidate reports one at zero has no `old` for the field that failed
/// and still reverts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The model keeps the entry the last accepted table gave it. The
    /// table stays priced; only this change is refused.
    KeptPriorPrice,
    /// The model is absent from the accepted table: there was no usable
    /// prior price to fall back to, so ADR-0004's at-use backstop is
    /// what refuses a dispatch that names it.
    NotAdmitted,
}

/// One model's proposed change, refused. Prices are per token, as the
/// source states them.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub model: String,
    /// The first field that failed, in [`PriceField::ORDER`].
    pub field: PriceField,
    /// What the last accepted table said *for this field*. `None` where
    /// it said nothing: a model with no prior entry, one whose prior
    /// price failed the plausibility floor, or a category the prior did
    /// not report at all.
    pub old: Option<f64>,
    /// What the candidate proposed.
    pub new: f64,
    /// `new / old` (or its reciprocal, whichever is the larger move).
    /// `None` where the change has no ratio: a model judged at
    /// admission, or a category the prior did not price.
    pub ratio: Option<f64>,
    pub rule: RefusalRule,
    /// What became of the model.
    pub disposition: Disposition,
}

impl Refusal {
    /// Whether this refusal dropped the model rather than reverting it.
    pub fn is_admission(&self) -> bool {
        self.disposition == Disposition::NotAdmitted
    }

    /// The one line an operator reads while deciding whether to open it.
    pub fn summary(&self) -> String {
        if self.is_admission() {
            return format!(
                "{} reports {} = {} and has no accepted price; not admitted to the table",
                self.model, self.field, self.new
            );
        }
        match (self.ratio, self.old) {
            (Some(ratio), _) => format!(
                "{} {} moved {:.1}x; kept the prior price",
                self.model, self.field, ratio
            ),
            (None, Some(old)) => format!(
                "{} {} moved {old} -> {}; kept the prior price",
                self.model, self.field, self.new
            ),
            (None, None) => format!(
                "{} newly reports {} = {}; kept the prior price",
                self.model, self.field, self.new
            ),
        }
    }
}

/// Judge `candidate` against the last accepted table and return what to
/// serve and what to say about it.
///
/// The result starts from the candidate and is *repaired*: a model whose
/// change is refused carries its prior price, a model refused at
/// admission is absent — a new one, or one whose prior price was not a
/// price at all — and everything else is the candidate's. Models
/// the candidate no longer lists are not carried over — a load is a safe
/// boundary, where a mid-flight refresh is not
/// (<https://github.com/bricef/factor-q/issues/344> owns that).
///
/// Provenance is cleared: the returned table is neither the prior nor
/// the candidate, and the loader stamps it with the digest of the bytes
/// it actually writes.
pub fn accept(
    prior: &PricingTable,
    candidate: PricingTable,
    rules: AcceptanceRules,
) -> (PricingTable, Vec<Refusal>) {
    let mut accepted = candidate;
    accepted.provenance = None;
    let mut refusals = Vec::new();
    // Sorted, so the refusal list — and the notifications built from it
    // — is a function of the two tables and not of hash iteration.
    let mut models: Vec<String> = accepted.entries.keys().cloned().collect();
    models.sort();
    for model in models {
        let proposed = accepted.entries[&model];
        // A prior that fails the plausibility floor is not a prior. It is
        // a price of zero (or worse) sitting in the cache — from a table
        // written before acceptance existed, or from a document that was
        // accepted when the floor was looser — and there is no ratio to
        // a zero, so bounding a move against it can only ever keep the
        // zero. The model is judged as if it were new instead: a
        // candidate that is still zero is refused at admission and
        // dropped, exactly as it would have been on a clean cache, and
        // one that now carries a real price is admitted on plausibility
        // alone.
        match prior
            .entries
            .get(&model)
            .filter(|previous| judge_admission(&model, previous).is_none())
        {
            Some(previous) => {
                if let Some(refusal) = judge_change(&model, previous, &proposed, rules) {
                    accepted.entries.insert(model, *previous);
                    refusals.push(refusal);
                }
            }
            None => {
                if let Some(refusal) = judge_admission(&model, &proposed) {
                    accepted.entries.remove(&model);
                    refusals.push(refusal);
                }
            }
        }
    }
    (accepted, refusals)
}

/// A model that was priced before: the bound and the zero rule.
///
/// `previous` is a *usable* prior — one that passes the plausibility
/// floor — because [`accept`] judges a model whose prior does not at
/// admission instead. That is what makes "the ratio to the prior price"
/// a number and not a division by zero.
fn judge_change(
    model: &str,
    previous: &ModelPricing,
    proposed: &ModelPricing,
    rules: AcceptanceRules,
) -> Option<Refusal> {
    for field in PriceField::ORDER {
        let new = field.per_million(proposed);
        let old = field.per_million(previous);
        let refusal = |rule, ratio| {
            Some(Refusal {
                model: model.to_string(),
                field,
                old: field.per_token(previous),
                new: field.per_token(proposed).unwrap_or(0.0),
                ratio,
                rule,
                disposition: Disposition::KeptPriorPrice,
            })
        };
        let (Some(new_price), Some(old_price)) = (new, old) else {
            // The source dropped (or newly published) a cache rate.
            // Dropping one is not a price change to refuse: a model
            // without published cache rates is charged at the base input
            // rate, which never under-bills. Publishing one is judged
            // for plausibility only — there is no prior to bound it
            // against.
            if let Some(new_price) = new
                && !is_positive(new_price)
            {
                return refusal(RefusalRule::ZeroPrice, None);
            }
            continue;
        };
        if !is_positive(new_price) {
            // A change to zero, negative or NaN. `previous` passed the
            // plausibility floor before this ran, so `old_price` is a
            // real price and this is always a refusal.
            return refusal(RefusalRule::ZeroPrice, None);
        }
        if !is_positive(old_price) {
            // Unreachable through [`accept`], which judges a model whose
            // prior fails the floor at admission instead. Kept as the
            // safe answer for any other caller: a move off zero has no
            // ratio to measure it by, so the operator sees the pair of
            // numbers and decides.
            return refusal(RefusalRule::DriftBound, None);
        }
        let ratio = (new_price / old_price).max(old_price / new_price);
        if ratio > rules.max_drift_ratio {
            return refusal(RefusalRule::DriftBound, Some(ratio));
        }
    }
    None
}

/// A model with no usable prior: the plausibility floor. Every token
/// category it reports must carry a positive price, or it is not
/// admitted. Also the test for whether a prior *is* usable — a cached
/// entry that would not be admitted today is not a price to judge a
/// change against.
fn judge_admission(model: &str, proposed: &ModelPricing) -> Option<Refusal> {
    for field in PriceField::ORDER {
        let Some(price) = field.per_token(proposed) else {
            continue;
        };
        if !is_positive(price) {
            return Some(Refusal {
                model: model.to_string(),
                field,
                old: None,
                new: price,
                ratio: None,
                rule: RefusalRule::ZeroPrice,
                disposition: Disposition::NotAdmitted,
            });
        }
    }
    None
}

/// A price is plausible when it is a positive, finite number. Zero is
/// the case the rule is named for; negative and NaN are the same
/// nonsense arriving by a different route.
fn is_positive(price: f64) -> bool {
    price.is_finite() && price > 0.0
}

/// Splice the refusals back into the document that will be cached.
///
/// [`accept`] decides on prices; the cache holds *bytes*, and they have
/// to say the same thing. Rather than re-serialising parsed floats —
/// which would put every price through per-million and back, and move
/// the digest on a daemon restart that changed nothing — the accepted
/// document is the fetched one with each refused model's entry taken
/// from the prior document, or removed where the model was refused at
/// admission. Fields the table never parses (`litellm_provider`, `mode`)
/// therefore stay exactly as upstream wrote them.
pub(crate) fn accepted_document(
    prior: &Map<String, Value>,
    fetched: Map<String, Value>,
    refusals: &[Refusal],
) -> Map<String, Value> {
    let mut document = fetched;
    for refusal in refusals {
        // `is_admission()` — not "is the model in the prior document?" —
        // decides. A model whose prior price failed the floor *is* in
        // that document, and splicing it back would put the $0 the table
        // just refused into the bytes the daemon serves next time.
        match prior
            .get(&refusal.model)
            .filter(|_| !refusal.is_admission())
        {
            Some(entry) => {
                document.insert(refusal.model.clone(), entry.clone());
            }
            None => {
                document.remove(&refusal.model);
            }
        }
    }
    document
}

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;
