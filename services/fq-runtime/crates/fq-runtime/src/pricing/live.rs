//! The live table's load path: fetch, accept, cache, and say what
//! happened.
//!
//! [`accept`] is the decision; this is the plumbing around it
//! (<https://github.com/bricef/factor-q/issues/735>). In order:
//!
//! 1. Read the **last accepted table** off the cache — the cache holds
//!    accepted tables only, which is what makes "the last accepted
//!    table" a file rather than a notion. Its provenance sits beside it
//!    in a sidecar.
//! 2. Fetch the configured source: LiteLLM's `main` document, or a
//!    pinned commit's copy of it.
//! 3. Put the two through acceptance.
//! 4. Write what was accepted — the fetched document with each refused
//!    model's entry taken from the prior one — and record its
//!    provenance: the upstream commit, the fetched blob's `ETag`, and a
//!    SHA256 digest of exactly those bytes.
//! 5. Report: one notification per model whose *change* was refused,
//!    one for a failed fetch, and an alert when the table being served
//!    is older than `[pricing] max_age`. A model refused at admission
//!    is counted into the log line rather than published — see
//!    [`AcceptedLoad::signals`].
//!
//! **Nothing here fails.** The runtime does not block on pricing; the
//! startup guarantee (ADR-0004) decides afterwards whether the result
//! suffices, and it is the thing that refuses to run.
//!
//! The periodic refresh (<https://github.com/bricef/factor-q/issues/344>)
//! is not here. It calls [`accept`] on its own schedule with its own
//! boundary rules; this path is what happens at startup, where there is
//! nothing in flight to widen for.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::accept::{AcceptanceRules, Refusal, accept, accepted_document};
use super::{PricingError, PricingTable};
use crate::events::operator_signal::kinds;
use crate::events::{OperatorSignalPayload, PricingProvenance, SignalKind};

/// The GitHub contents API query that answers "what commit last touched
/// this file?" — one entry, newest first.
const LITELLM_COMMITS_URL: &str = "https://api.github.com/repos/BerriAI/litellm/commits\
     ?path=model_prices_and_context_window.json&per_page=1";

/// The document's path in the LiteLLM repository, for a pinned fetch.
const LITELLM_FILE: &str = "model_prices_and_context_window.json";

/// How long the accepted table may go without a refresh before the load
/// raises `pricing.stale`. Seven days: long enough that a weekend of a
/// broken fetch is a notification rather than a page, short enough that
/// prices older than the models they price are never silent.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Which upstream document the live table is read from.
///
/// **The default is live, and pinning is not deprecated.** An air-gapped
/// or regulated deployment pins a commit and reviews the diff itself;
/// everyone else takes the live document and lets acceptance be the
/// discipline. What a pin must not do is become the default: LiteLLM
/// adds models weekly, and under ADR-0004 a model with no price is a
/// daemon that will not start, so a pinned table turns "prices drift"
/// into "new models are refused until a human bumps a SHA".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TableSource {
    /// LiteLLM's `main` branch — the live document.
    #[default]
    LitellmMain,
    /// One commit of it, forever.
    Pinned(String),
}

impl TableSource {
    /// Parse the `[pricing] source` setting: `litellm-main` or
    /// `pinned:<sha>`.
    pub fn parse(setting: &str) -> Result<Self, PricingError> {
        match setting.trim() {
            "litellm-main" => Ok(Self::LitellmMain),
            other => match other.strip_prefix("pinned:") {
                Some(sha) if is_commit_sha(sha) => Ok(Self::Pinned(sha.to_string())),
                Some(sha) => Err(PricingError::Source(format!(
                    "`pinned:{sha}` is not a commit sha (hex, 7-40 characters)"
                ))),
                None => Err(PricingError::Source(format!(
                    "`{other}` is not a pricing source: expected `litellm-main` or `pinned:<sha>`"
                ))),
            },
        }
    }

    /// Where the document is fetched from.
    pub fn url(&self) -> String {
        match self {
            Self::LitellmMain => super::LITELLM_PRICING_URL.to_string(),
            Self::Pinned(sha) => {
                format!("https://raw.githubusercontent.com/BerriAI/litellm/{sha}/{LITELLM_FILE}")
            }
        }
    }

    /// The commit this source names, when naming one is what it is for.
    /// A pin knows its commit without asking anybody.
    pub fn commit(&self) -> Option<&str> {
        match self {
            Self::LitellmMain => None,
            Self::Pinned(sha) => Some(sha),
        }
    }
}

impl std::fmt::Display for TableSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LitellmMain => f.write_str("litellm-main"),
            Self::Pinned(sha) => write!(f, "pinned:{sha}"),
        }
    }
}

fn is_commit_sha(candidate: &str) -> bool {
    (7..=40).contains(&candidate.len()) && candidate.chars().all(|c| c.is_ascii_hexdigit())
}

/// What the operator's `[pricing]` section resolves to.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadSettings {
    pub source: TableSource,
    pub rules: AcceptanceRules,
    /// How old the accepted table may be before the load alerts.
    pub max_age: Duration,
}

impl Default for LoadSettings {
    fn default() -> Self {
        Self {
            source: TableSource::default(),
            rules: AcceptanceRules::default(),
            max_age: DEFAULT_MAX_AGE,
        }
    }
}

/// An accepted table older than its window, at the moment it was loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Staleness {
    /// When the table now being served was accepted.
    pub accepted_at: DateTime<Utc>,
    /// How long ago that was.
    pub age: Duration,
    /// The window it exceeded.
    pub max_age: Duration,
}

/// What one load produced: the table to serve, and everything an
/// operator should be told about it.
#[derive(Debug)]
pub struct AcceptedLoad {
    /// The table to serve. Stamped with its provenance when there was
    /// one to stamp.
    pub table: PricingTable,
    /// One entry per model whose proposed change was refused.
    pub refusals: Vec<Refusal>,
    /// Set when the table being served is past its `max_age`.
    pub staleness: Option<Staleness>,
    /// Why the fetch did not happen, when it did not. Last-known-good is
    /// already being served; this says the daemon is doing that.
    pub fetch_error: Option<String>,
}

impl AcceptedLoad {
    /// What to publish about this load: one notification per model whose
    /// *change* was refused, one for a failed fetch, and an alert for a
    /// table past its window.
    ///
    /// Severity is a property of the situation. A refusal is a
    /// notification because the daemon carries on at the prior price and
    /// nothing is broken; so is a failed fetch, for the same reason. A
    /// table that has stopped refreshing is an alert: no further attempt
    /// recovers a source that has stopped answering, and prices silently
    /// older than the models they price is the failure ADR-0004's
    /// guarantee exists to prevent.
    pub fn signals(&self) -> Vec<OperatorSignalPayload> {
        let mut signals = Vec::new();
        if let Some(error) = &self.fetch_error {
            signals.push(
                OperatorSignalPayload::notification(
                    SignalKind::registered(kinds::PRICING_FETCH_FAILED),
                    format!("pricing fetch failed; serving the last accepted table ({error})"),
                )
                .with_detail(json!({ "error": error })),
            );
        }
        // A refused *change* is news: something that was priced moved
        // implausibly, and one line per model is what an operator wants.
        // A refused *admission* is not. The live table lists several
        // hundred free, local and embedding entries priced at zero, so
        // on every start every one of them is a new model that fails the
        // floor — a standing property of the source, identical on each
        // load, and nothing an operator can act on. Those are counted
        // into the load's log line instead (see `settle`), and the
        // moment one of them matters — something declares it — ADR-0004's
        // startup guarantee refuses to run and names it, which is louder
        // than any notification.
        for refusal in self.refusals.iter().filter(|r| !r.is_admission()) {
            signals.push(refusal_signal(refusal));
        }
        if let Some(stale) = &self.staleness {
            signals.push(
                OperatorSignalPayload::alert(
                    SignalKind::registered(kinds::PRICING_STALE),
                    format!(
                        "the pricing table has not refreshed for {} days",
                        stale.age.as_secs() / 86_400
                    ),
                )
                .with_detail(json!({
                    "last_refresh_ms": stale.accepted_at.timestamp_millis(),
                    "window_hours": stale.max_age.as_secs() / 3_600,
                })),
            );
        }
        signals
    }
}

/// One refused change, as the operator-signal registry documents it:
/// model, field, old, new, ratio, rule. Prices are per token, as the
/// source states them, so the numbers match the file an operator opens.
fn refusal_signal(refusal: &Refusal) -> OperatorSignalPayload {
    OperatorSignalPayload::notification(
        SignalKind::registered(kinds::PRICING_CHANGE_REFUSED),
        refusal.summary(),
    )
    .with_detail(json!({
        "model": refusal.model,
        "field": refusal.field.as_str(),
        "old": refusal.old,
        "new": refusal.new,
        "ratio": refusal.ratio,
        "rule": refusal.rule.as_str(),
    }))
}

/// Fetch the configured document, accept it against the last accepted
/// table, cache what was accepted, and report.
///
/// Never fails. A fetch that does not land leaves the last accepted
/// table in place and says so.
pub async fn load_accepted(settings: LoadSettings, cache_path: &Path) -> AcceptedLoad {
    let fetched = fetch_document(&settings.source).await;
    let commit = match (&fetched, settings.source.commit()) {
        // A pin knows its own commit; asking GitHub would only confirm
        // the sha we already fetched by.
        (Ok(_), Some(pinned)) => Some(pinned.to_string()),
        // Best-effort, and nothing stands in for it: the blob's `ETag`
        // is recorded as an `ETag` rather than passed off as a commit.
        (Ok(_), None) => upstream_commit().await,
        (Err(_), _) => None,
    };
    settle(&settings, cache_path, fetched, commit, Utc::now())
}

/// One document off the wire.
struct FetchedDocument {
    body: String,
    /// The raw URL's `ETag`: the CDN's identifier for these exact bytes.
    /// Recorded in the provenance beside the commit, never in place of
    /// it — the two answer different questions, and a short hex `ETag`
    /// is indistinguishable in shape from a sha.
    etag: Option<String>,
}

/// Everything the load does once the network is out of the way: the
/// disk, the acceptance, and the verdicts.
///
/// Separated so the whole path is testable without a socket — the HTTP
/// above is a fetch and a header, and every decision worth asserting on
/// is below.
fn settle(
    settings: &LoadSettings,
    cache_path: &Path,
    fetched: Result<FetchedDocument, PricingError>,
    commit: Option<String>,
    now: DateTime<Utc>,
) -> AcceptedLoad {
    let cached = read_cache(cache_path);
    let prior_document = cached
        .as_ref()
        .map(|c| c.document.clone())
        .unwrap_or_default();
    let prior = PricingTable::from_litellm_document(&prior_document);

    let fetched = match fetched {
        Ok(document) => document,
        Err(err) => return last_known_good(prior, &cached, settings, now, &err),
    };

    let document: Map<String, Value> = match serde_json::from_str(&fetched.body) {
        Ok(document) => document,
        Err(err) => {
            // A malformed document is a failed fetch by another name:
            // the daemon has nothing new to accept, and keeps what it
            // had.
            let err = PricingError::Parse(err.to_string());
            return last_known_good(prior, &cached, settings, now, &err);
        }
    };

    let candidate = PricingTable::from_litellm_document(&document);
    let (accepted, refusals) = accept(&prior, candidate, settings.rules);
    let accepted_doc = accepted_document(&prior_document, document, &refusals);
    let bytes = canonical_bytes(&accepted_doc);
    let provenance = Some(PricingProvenance {
        source: settings.source.to_string(),
        commit,
        etag: fetched.etag,
        digest: digest_of(&bytes),
        accepted_at: now,
    });
    write_cache(cache_path, &bytes, provenance.as_ref());
    let (not_admitted, changes): (Vec<&Refusal>, Vec<&Refusal>) =
        refusals.iter().partition(|r| r.is_admission());
    info!(
        entries = accepted.len(),
        refused_changes = changes.len(),
        not_admitted = not_admitted.len(),
        source = %settings.source,
        "accepted a pricing table"
    );
    if !not_admitted.is_empty() {
        // Names at debug, count at info: several hundred of these is
        // normal, and the list is only wanted when somebody is asking
        // why a particular model is unpriced.
        debug!(
            models = ?not_admitted.iter().map(|r| r.model.as_str()).collect::<Vec<_>>(),
            "models not admitted: not priced on every category they report"
        );
    }
    AcceptedLoad {
        table: stamp(accepted, provenance),
        refusals,
        // A table accepted just now is not stale, whatever the last one
        // was: the window is about the table being served.
        staleness: None,
        fetch_error: None,
    }
}

/// Nothing new landed: serve what was accepted last time, and say so.
///
/// This is where the staleness window is read, and the only place it
/// can fire at startup — a table accepted moments ago is not old.
fn last_known_good(
    prior: PricingTable,
    cached: &Option<CachedTable>,
    settings: &LoadSettings,
    now: DateTime<Utc>,
    err: &PricingError,
) -> AcceptedLoad {
    match cached {
        Some(_) => warn!(
            error = %err,
            entries = prior.len(),
            "failed to fetch pricing; serving the last accepted table"
        ),
        None => warn!(
            error = %err,
            "failed to fetch pricing and no accepted table is cached; \
             models it would have priced are unpriced"
        ),
    }
    let provenance = cached.as_ref().and_then(|c| c.provenance.clone());
    AcceptedLoad {
        staleness: staleness(&provenance, now, settings.max_age),
        table: stamp(prior, provenance),
        refusals: Vec::new(),
        fetch_error: Some(err.to_string()),
    }
}

fn stamp(table: PricingTable, provenance: Option<PricingProvenance>) -> PricingTable {
    match provenance {
        Some(provenance) => table.with_provenance(provenance),
        None => table,
    }
}

/// Whether the table being served is past its window. Pure, so the
/// clock is an argument rather than a dependency.
fn staleness(
    provenance: &Option<PricingProvenance>,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Option<Staleness> {
    let accepted_at = provenance.as_ref()?.accepted_at;
    let age = now.signed_duration_since(accepted_at).to_std().ok()?;
    (age > max_age).then_some(Staleness {
        accepted_at,
        age,
        max_age,
    })
}

/// The last accepted table, as bytes and as what is known about them.
struct CachedTable {
    document: Map<String, Value>,
    provenance: Option<PricingProvenance>,
}

/// Where the provenance of the cached table lives — beside it, so an
/// operator looking at `pricing.json` finds `pricing.provenance.json`
/// next to it and can see which document it came from.
fn provenance_path(cache_path: &Path) -> PathBuf {
    cache_path.with_extension("provenance.json")
}

fn read_cache(cache_path: &Path) -> Option<CachedTable> {
    let bytes = fs::read(cache_path).ok()?;
    let document: Map<String, Value> = serde_json::from_slice(&bytes)
        .map_err(|err| warn!(error = %err, path = %cache_path.display(), "the cached pricing table is corrupt; ignoring it"))
        .ok()?;
    // The digest names the bytes. A sidecar that disagrees with the file
    // beside it describes some other table — a hand-edited cache, a
    // half-written pair — so the provenance is dropped rather than
    // believed, and the table reloads as one with no history.
    let provenance = fs::read_to_string(provenance_path(cache_path))
        .ok()
        .and_then(|json| serde_json::from_str::<PricingProvenance>(&json).ok())
        .filter(|provenance| {
            let matches = provenance.digest == digest_of(&bytes);
            if !matches {
                warn!(
                    path = %cache_path.display(),
                    "the cached pricing table does not match its recorded digest; treating it as unattributed"
                );
            }
            matches
        });
    Some(CachedTable {
        document,
        provenance,
    })
}

fn write_cache(cache_path: &Path, bytes: &[u8], provenance: Option<&PricingProvenance>) {
    if let Some(parent) = cache_path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        warn!(error = %err, "failed to create the pricing cache directory");
        return;
    }
    if let Err(err) = fs::write(cache_path, bytes) {
        warn!(error = %err, "failed to write the pricing cache");
        return;
    }
    let Some(provenance) = provenance else { return };
    match serde_json::to_vec_pretty(provenance) {
        Ok(json) => {
            if let Err(err) = fs::write(provenance_path(cache_path), json) {
                warn!(error = %err, "failed to write the pricing provenance");
            }
        }
        Err(err) => warn!(error = %err, "failed to serialise the pricing provenance"),
    }
}

/// The bytes a table is cached and digested as: models in name order.
///
/// Sorted explicitly rather than left to `serde_json`, because
/// `serde_json`'s map is insertion-ordered in this workspace — `genai`
/// enables `preserve_order`, and a feature is unified across the whole
/// dependency graph. Without the sort the digest would depend on the
/// order two documents happened to be spliced in, and a splice puts a
/// prior entry back where the fetched document had it. Sorting the top
/// level also makes the cache file diffable, which is the other thing an
/// operator does with it. Field order *inside* an entry stays as
/// upstream wrote it: the entry is copied verbatim, which is the point.
fn canonical_bytes(document: &Map<String, Value>) -> Vec<u8> {
    let sorted: std::collections::BTreeMap<&String, &Value> = document.iter().collect();
    serde_json::to_vec_pretty(&sorted).unwrap_or_default()
}

/// SHA256, lowercase hex.
fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn fetch_document(source: &TableSource) -> Result<FetchedDocument, PricingError> {
    let client = super::http_client()?;
    let url = source.url();
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| PricingError::Http(err.to_string()))?;
    if !response.status().is_success() {
        return Err(PricingError::Http(format!(
            "unexpected status: {}",
            response.status()
        )));
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"').to_string());
    let body = response
        .text()
        .await
        .map_err(|err| PricingError::Http(err.to_string()))?;
    debug!(bytes = body.len(), %url, "fetched pricing document");
    Ok(FetchedDocument { body, etag })
}

/// The commit that last touched the upstream document, from the GitHub
/// contents API — the file's latest commit *at this instant*, which is
/// the newest commit the fetched bytes could have come from and may be
/// one ahead of them.
///
/// Best-effort: unauthenticated, on the same short timeout as the
/// document fetch, and `None` on anything unexpected. The table is still
/// identified by its digest without it; what is missing is the upstream
/// history to look that digest up in, and no operator wants a daemon
/// that will not start because api.github.com was rate-limiting.
async fn upstream_commit() -> Option<String> {
    let client = super::http_client().ok()?;
    let response = client
        .get(LITELLM_COMMITS_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        debug!(status = %response.status(), "the commits API did not answer; the table is identified by its digest and ETag");
        return None;
    }
    let commits: Vec<Value> = response.json().await.ok()?;
    let sha = commits.first()?.get("sha")?.as_str()?;
    is_commit_sha(sha).then(|| sha.to_string())
}

#[cfg(test)]
mod tests;
