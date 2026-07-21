//! The declared surface: what stays hand-written because it is
//! semantically bespoke — the domain verbs and the reports. Everything
//! else derives from the catalogue, and that division is the model's
//! own line: what remains declared is exactly what a generic verb
//! would bury.
//!
//! A declaration is **one site**: the impl carries its own identity
//! (the resource it attaches to and its verb name, or a report's
//! name), its types, its authority, and its contract text. Adding a
//! verb is writing the impl and registering it — no enum to extend,
//! no match to update, nowhere else to touch. Identity collisions are
//! caught at registration (the trivial test the strings owe us).

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::catalogue::Domain;
use crate::meta::{Authority, Stability};
use crate::opid::OpId;

/// A bespoke command, attached to a resource — machinery verbs attach
/// to the synthetic `Control` resource. Its output is always a
/// [`crate::wire::Receipt`] — commands return references to the atoms
/// they appended, never state (D3); there is no `Output` to declare,
/// so the rule cannot be broken. Authority is declared, not derived:
/// the semantics that make a verb bespoke are exactly what generic
/// derivation would get wrong.
pub trait Command {
    const DOMAIN: Domain;
    /// The verb word itself; renders as `{resource}.{verb}`. Opaque
    /// identity plus documentation — never parsed.
    const VERB: &'static str;
    const VERSION: u32 = 1;
    type Input: Serialize + DeserializeOwned + JsonSchema;
    const AUTHORITY: Authority;
    /// One-line description of the command.
    const DESCRIPTION: &'static str;
    const STABILITY: Stability;
    /// The contract text that makes this verb bespoke: idempotency,
    /// kill-switch semantics, delivery guarantees. Defaults to "none".
    const CAVEATS: &'static str = "";

    /// This command's wire identity.
    fn op() -> OpId {
        OpId::Verb {
            domain: Self::DOMAIN,
            verb: Self::VERB.to_string(),
        }
    }
}

/// A named, typed computation over resources — the kind the original
/// taxonomy was missing. Not a Get on a pretend-resource and not a
/// query language: few by design, watermarked like any read. `READS`
/// declares the resource scopes the computation consumes; authority
/// is Read on each.
pub trait Report {
    /// The report's full rendered name (`cost.summary`). Reports are
    /// not resource-attached, so the name is free-standing — declared
    /// here, never parsed.
    const NAME: &'static str;
    const VERSION: u32 = 1;
    type Params: Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const READS: &'static [Domain];
    /// One-line description of the report.
    const DESCRIPTION: &'static str;
    const STABILITY: Stability;
    const CAVEATS: &'static str = "";

    /// This report's wire identity.
    fn op() -> OpId {
        OpId::Report {
            name: Self::NAME.to_string(),
        }
    }
}
