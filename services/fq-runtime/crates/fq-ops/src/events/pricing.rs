//! Which pricing table priced a run — the provenance every spend figure
//! is traceable to.
//!
//! The cost-retention principle keeps cost information indefinitely, and
//! a figure nobody can attach to a price list is a number, not a record.
//! So the accepted table names itself: what source it was configured
//! from, the upstream commit and blob `ETag` the document was read at,
//! and a SHA256 digest of the exact bytes the daemon accepted
//! (<https://github.com/bricef/factor-q/issues/735>, rule 4).
//!
//! **One value, two uses.** The whole provenance rides the
//! `system.startup` event once per daemon run; every cost record cites
//! the short [`version`](PricingProvenance::version) derived from it, so
//! a row says which table priced it without repeating a 64-character
//! digest on every LLM call. The short form resolves against the startup
//! event of the run that wrote the row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How many hex characters of the digest the short version carries.
///
/// Twelve: enough that two tables in one deployment's history colliding
/// is not a thing that happens, short enough to read in a table cell
/// beside a model name.
const VERSION_DIGEST_CHARS: usize = 12;

/// Where an accepted pricing table came from, and exactly which one it
/// is.
///
/// Written by the acceptance step in `fq_runtime::pricing`, carried on
/// `system.startup`, and cached beside the table itself so a daemon that
/// falls back to the last accepted copy still knows what it is serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingProvenance {
    /// The configured source, spelled as the config spells it:
    /// `litellm-main` or `pinned:<sha>`.
    pub source: String,
    /// The file's latest commit at fetch time, best-effort: the GitHub
    /// contents API's newest commit touching the upstream document, or
    /// the sha a pinned source names.
    ///
    /// **Not a claim about these bytes.** The document comes from
    /// `raw.githubusercontent.com`, which is CDN-cached, and the sha is
    /// asked for at a later instant, so a commit landing in between
    /// leaves this one commit ahead of the bytes. What identifies the
    /// table is the [`digest`](Self::digest); this is the upstream
    /// history to read it against.
    ///
    /// `None` where the API did not answer — unauthenticated and
    /// rate-limited is the common case — which is why the [`etag`](Self::etag)
    /// is recorded separately rather than substituted here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The raw URL's `ETag`: the CDN's identifier for the blob that was
    /// actually fetched, which is what makes it worth keeping beside a
    /// commit sha that may be newer.
    ///
    /// `None` where the response carried no `ETag`, or where nothing was
    /// fetched at all. A short hex `ETag` is indistinguishable in shape
    /// from a commit sha, which is the reason these are two fields: a
    /// reader looking up `commit` in the upstream history must never be
    /// handed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// SHA256, lowercase hex, of the accepted table's canonical bytes —
    /// the bytes in the cache, which are what the daemon accepted rather
    /// than what the source offered. The two differ exactly when
    /// acceptance refused a change and kept a prior price.
    pub digest: String,
    /// When the daemon accepted this table. Read as an age at the next
    /// load: a table older than `[pricing] max_age` raises
    /// `pricing.stale`.
    pub accepted_at: DateTime<Utc>,
}

impl PricingProvenance {
    /// The short reference a cost record cites: `<source>@<digest12>`.
    ///
    /// Content-addressed, so a daemon that restarts onto an unchanged
    /// table writes the same version — which is the property that makes
    /// "did the prices move between these two runs?" answerable by
    /// comparing two strings.
    pub fn version(&self) -> String {
        let short: String = self.digest.chars().take(VERSION_DIGEST_CHARS).collect();
        format!("{}@{}", self.source, short)
    }
}

#[cfg(test)]
mod tests;
