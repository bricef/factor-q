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
//! 2. **Zero refusal.** A model priced at zero where it was not is
//!    refused the same way. A *new* model priced at zero is refused at
//!    admission — never entering the table at all, which leaves
//!    ADR-0004's at-use backstop to refuse the dispatch rather than
//!    letting it run at $0.
//! 3. **Plausibility floor for new models.** A model with no prior is
//!    accepted only if every token category it reports carries a
//!    positive price.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceField {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl PriceField {
    /// The upstream field name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input_cost_per_token",
            Self::Output => "output_cost_per_token",
            Self::CacheRead => "cache_read_input_token_cost",
            Self::CacheWrite => "cache_creation_input_token_cost",
        }
    }

    /// The order fields are judged in, which is the order a refusal
    /// picks its field from. Fixed so the same pair of tables always
    /// produces the same refusal.
    pub const ORDER: [PriceField; 4] =
        [Self::Input, Self::Output, Self::CacheRead, Self::CacheWrite];

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

/// One model's proposed change, refused. Prices are per token, as the
/// source states them.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub model: String,
    /// The first field that failed, in [`PriceField::ORDER`].
    pub field: PriceField,
    /// What the last accepted table said. `None` for a new model
    /// refused at admission — there is no prior price, which is why the
    /// model is dropped rather than reverted.
    pub old: Option<f64>,
    /// What the candidate proposed.
    pub new: f64,
    /// `new / old` (or its reciprocal, whichever is the larger move).
    /// `None` where the change has no ratio: a new model, or a prior
    /// price of zero.
    pub ratio: Option<f64>,
    pub rule: RefusalRule,
}

impl Refusal {
    /// Whether this refusal dropped the model rather than reverting it —
    /// true exactly when there was no prior price to revert to.
    pub fn is_admission(&self) -> bool {
        self.old.is_none()
    }

    /// The one line an operator reads while deciding whether to open it.
    pub fn summary(&self) -> String {
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
                "{} is new and reports {} = {}; not admitted to the table",
                self.model, self.field, self.new
            ),
        }
    }
}

/// Judge `candidate` against the last accepted table and return what to
/// serve and what to say about it.
///
/// The result starts from the candidate and is *repaired*: a model whose
/// change is refused carries its prior price, a new model refused at
/// admission is absent, and everything else is the candidate's. Models
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
        match prior.entries.get(&model) {
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
            // A change to zero, negative or NaN. `old` zero and `new`
            // zero is not a change at all, and falls through.
            if is_positive(old_price) {
                return refusal(RefusalRule::ZeroPrice, None);
            }
            continue;
        }
        if !is_positive(old_price) {
            // Zero to something: a real move with no ratio to measure it
            // by. Refused as drift — the operator sees the pair of
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

/// A model with no prior: the plausibility floor. Every token category
/// it reports must carry a positive price, or it is not admitted.
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
        match prior.get(&refusal.model) {
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
