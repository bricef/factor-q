//! The client's own configuration — `fq.toml`.
//!
//! Named for the binary that reads it. The daemon's is `fqd.toml`, and
//! neither has to know the other's shape; a client that read the
//! daemon's config could not work against a remote daemon at all, since
//! the operator on another machine has no such file.
//!
//! **The file is optional, and on day one holds at most one setting.**
//! That is not an oversight. `fq connect` already writes the pairing
//! store, keyed by address, so the client knows every daemon it can
//! reach and how to authenticate to each. The only thing left to say is
//! *which* of them to use when there is more than one — and with a
//! single pairing, even that is answered.
//!
//! Credentials stay in `connections.toml` rather than moving here: a
//! token rotates when the daemon's identity does, on a different
//! schedule from anything an operator would edit, and merging the two
//! means either this file inherits 0600 or the secrets stop being
//! private.

use std::path::Path;

/// `fq.toml`, as far as the client is concerned.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct ClientConfig {
    #[serde(default)]
    pub(crate) daemon: DaemonSelection,
}

/// Which daemon this client talks to by default.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct DaemonSelection {
    /// The edge address of the daemon to use when `--addr` is absent
    /// and more than one pairing exists. Names a pairing rather than
    /// carrying credentials, so rotating a token never touches this
    /// file.
    #[serde(default)]
    pub(crate) addr: Option<String>,
}

impl ClientConfig {
    /// Read `fq.toml` if it is there.
    ///
    /// A missing file is the healthy case, not a degraded one: a client
    /// with one pairing has nothing to configure. Only a file that
    /// exists and cannot be parsed is an error, because that is an
    /// operator's edit that did not take effect and they should hear
    /// about it rather than watch it be ignored.
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(anyhow::anyhow!("{}: {err}", path.display())),
        }
    }
}
