//! The daemon's build stamp.
//!
//! Only the one string: `fqd --version` is clap's, and `control.status`
//! answers with this. The client prints its own build from its own
//! crate — two binaries, two stamps, and a deploy that mixes them is
//! visible rather than inferred.

/// Semver plus the commit it was built from.
pub(crate) const FQ_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("FQ_GIT_SHA"));
