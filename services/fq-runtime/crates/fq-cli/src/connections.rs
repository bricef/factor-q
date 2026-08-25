//! The client's side of the edge pairing: the credential store, `fq connect`,
//! and the two verbs that read a stored pairing directly (`fq ops list`,
//! `fq token attenuate`).
//!
//! Split out of `lib.rs` (#189). Credentials live user-side in
//! `~/.config/factor-q/connections.toml` (0600) — never in the project's
//! fq.toml, which is shared and committed. `edge_call.rs` dials with what is
//! stored here; nothing here knows about a verb's payload.

use std::path::{Path, PathBuf};

use anyhow::Context;

use fq_edge::{fingerprint_hex, parse_fingerprint_hex};

use crate::cli::GlobalArgs;

// ---------------------------------------------------------------------
// The edge client side (ADR-0031 Appendix A, plan Phase 2 2b): pairing
// with a daemon (`fq connect`), the first command over the
// authenticated surface (`fq ops list`), and offline attenuation
// (`fq token attenuate`). Credentials live user-side in
// `~/.config/factor-q/connections.toml` (0600) — never in the
// project's fq.toml, which is shared and committed.
// ---------------------------------------------------------------------

/// The client-side credential store: one pinned, tokened pairing per
/// edge address.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Connections {
    #[serde(default)]
    connections: std::collections::BTreeMap<String, ConnectionEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct ConnectionEntry {
    /// SHA-256 of the daemon's certificate, hex — the pin.
    pub(crate) fingerprint: String,
    /// The capability token presented in the connection preamble.
    pub(crate) token: String,
}

/// `$XDG_CONFIG_HOME/factor-q/connections.toml`, falling back to
/// `~/.config/factor-q/connections.toml`.
fn connections_path() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .ok_or_else(|| anyhow::anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("factor-q").join("connections.toml"))
}

fn load_connections(path: &Path) -> anyhow::Result<Connections> {
    if !path.exists() {
        return Ok(Connections::default());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

/// Create the credential directory owner-only.
fn ensure_config_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Insert one pairing under an exclusive advisory lock, re-reading the
/// store inside it. The window between a `connect`'s first read and
/// its write is seconds long (probe, handshake, possibly a human at
/// the [y/N] prompt), so writing that early snapshot back would
/// silently discard whatever a concurrent `fq connect` stored for
/// another address in the meantime — the lock makes every writer merge
/// into the latest state instead.
fn store_connection(path: &Path, addr: &str, entry: ConnectionEntry) -> anyhow::Result<()> {
    let dir = path.parent().expect("connections path has a parent");
    ensure_config_dir(dir)?;
    let lock_path = dir.join(".connections.lock");
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.lock()
        .map_err(|e| anyhow::anyhow!("lock {}: {e}", lock_path.display()))?;
    let mut store = load_connections(path)?;
    store.connections.insert(addr.to_string(), entry);
    store_connections(path, &store)
    // The lock releases when `lock` drops.
}

/// Persist the credential store: directory 0700, file 0600 from the
/// first byte, written to a temp file and renamed so a crash never
/// leaves a partial credentials file.
fn store_connections(path: &Path, connections: &Connections) -> anyhow::Result<()> {
    let dir = path.parent().expect("connections path has a parent");
    ensure_config_dir(dir)?;
    let body = toml::to_string_pretty(connections)?;
    // Per-process temp name: concurrent `fq connect` runs must not
    // race each other's staging file (the rename stays atomic either
    // way; this just removes the create_new collision).
    let tmp = dir.join(format!(".connections.toml.tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(body.as_bytes())?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `fq connect` — establish or refresh the pairing with a daemon's
/// edge. The pin comes from, in order: `--fingerprint`, the stored
/// entry, or TOFU (probe the server, show the fingerprint, and
/// confirm — interactively when stdin is a terminal, automatically
/// with a notice otherwise). A successful pinned connect proves both
/// the fingerprint and the token before anything is stored.
pub(crate) async fn connect(
    global: &GlobalArgs,
    addr: Option<String>,
    token: Option<String>,
    fingerprint_flag: Option<String>,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;

    let addr = match addr {
        Some(a) => a,
        None => crate::edge_call::daemon_addr(global)?,
    };
    let path = connections_path()?;
    let existing = load_connections(&path)?.connections.get(&addr).cloned();

    let token = token
        .or_else(|| existing.as_ref().map(|e| e.token.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no token for {addr}: pass --token (the daemon printed the admin token \
                 at first run)"
            )
        })?;

    let expected = if let Some(hex) = &fingerprint_flag {
        parse_fingerprint_hex(hex)?
    } else if let Some(entry) = &existing {
        parse_fingerprint_hex(&entry.fingerprint)?
    } else {
        // Trust on first use: nothing pinned yet for this address.
        let probed = fq_edge::probe_fingerprint(&addr).await?;
        let hex = fingerprint_hex(probed);
        eprintln!("The daemon at {addr} presents certificate fingerprint (SHA-256):");
        eprintln!("  {hex}");
        eprintln!(
            "Compare it with the fingerprint the daemon printed when it provisioned \
             its identity (the `edge: certificate fingerprint` line at first run)."
        );
        if std::io::stdin().is_terminal() {
            eprint!("Pin this fingerprint and continue? [y/N] ");
            {
                use std::io::Write;
                std::io::stderr().flush()?;
            }
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                anyhow::bail!("not pinned — aborted by operator");
            }
        } else {
            eprintln!("non-interactive: pinning automatically");
        }
        probed
    };

    edge_client(&addr, expected, &token).await?;

    store_connection(
        &path,
        &addr,
        ConnectionEntry {
            fingerprint: fingerprint_hex(expected),
            token,
        },
    )?;
    println!(
        "connected: {addr} (fingerprint {}…) — credentials stored in {}",
        &fingerprint_hex(expected)[..12],
        path.display()
    );
    Ok(())
}

/// Pinned connect with operator-grade error guidance for each
/// distinct refusal.
pub(crate) async fn edge_client(
    addr: &str,
    fingerprint: [u8; 32],
    token: &str,
) -> anyhow::Result<fq_edge::EdgeClient> {
    fq_edge::EdgeClient::connect(addr, fingerprint, token)
        .await
        .map_err(|e| match e {
            fq_edge::client::ConnectError::FingerprintMismatch => anyhow::anyhow!(
                "the daemon at {addr} presented a certificate that does not match the \
                 pinned fingerprint — possible interception, or the daemon's identity \
                 was rotated. If the rotation is expected, re-pin with \
                 `fq connect {addr} --token <token> --fingerprint <new-fingerprint>` \
                 after removing the entry from your connections.toml"
            ),
            fq_edge::client::ConnectError::TokenRejected => anyhow::anyhow!(
                "the daemon at {addr} rejected the token — it may have been minted \
                 under a rotated identity; obtain a fresh token from the daemon operator"
            ),
            fq_edge::client::ConnectError::Io(e) => {
                anyhow::anyhow!("could not reach the edge at {addr}: {e}")
            }
        })
}

/// Load the stored pairing for `addr`, with guidance when absent.
/// Every address this client has a pairing for, in stored order.
///
/// The client needs this to answer "which daemon" without being told:
/// one pairing is not ambiguous, and several are — the caller turns
/// that into a default or an error naming the choices.
pub(crate) fn paired_addresses() -> anyhow::Result<Vec<String>> {
    let path = connections_path()?;
    Ok(load_connections(&path)?.connections.into_keys().collect())
}

pub(crate) fn stored_connection(addr: &str) -> anyhow::Result<ConnectionEntry> {
    let path = connections_path()?;
    load_connections(&path)?
        .connections
        .get(addr)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no stored connection for {addr}: run `fq connect {addr} --token <token>` first"
            )
        })
}

/// `fq ops list` — the surface describing itself: `List(Operation)`
/// over the authenticated edge.
pub(crate) async fn ops_list(
    global: &GlobalArgs,
    addr: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let addr = match addr {
        Some(a) => a,
        None => crate::edge_call::daemon_addr(global)?,
    };
    let entry = stored_connection(&addr)?;
    let client = edge_client(
        &addr,
        parse_fingerprint_hex(&entry.fingerprint)?,
        &entry.token,
    )
    .await?;
    let response = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::List(fq_ops::Domain::Operation),
                version: 1,
                input: serde_json::json!({}),
                min_seq: None,
            },
        )
        .await
        .context("edge rpc failed")?
        .map_err(|e| anyhow::anyhow!("operation.list refused: {e:?}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&response.output)?);
        return Ok(());
    }
    let entries = response.output.as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        println!("no operations registered");
        return Ok(());
    }
    for entry in entries {
        // Each declaration serialises as a one-key object:
        // {"command": {"domain": .., "verb": .., "summary": ..}} etc.
        let Some((kind, body)) = entry.as_object().and_then(|o| o.iter().next()) else {
            println!("{entry}");
            continue;
        };
        let domain = body.get("domain").and_then(|v| v.as_str()).unwrap_or("?");
        let name = body
            .get("verb")
            .or_else(|| body.get("name"))
            .and_then(|v| v.as_str())
            .map(|n| format!("{domain}.{n}"))
            .unwrap_or_else(|| domain.to_string());
        let summary = body.get("summary").and_then(|v| v.as_str()).unwrap_or("");
        println!("{name:<30} {kind:<10} {summary}");
    }
    Ok(())
}

/// `fq token attenuate` — narrow a token offline. Grants arrive as
/// `verb:domain` with `*` wildcards; the attenuated token goes to
/// stdout alone, script-friendly.
pub(crate) fn token_attenuate(
    global: &GlobalArgs,
    grants: &[String],
    token: Option<String>,
    addr: Option<String>,
) -> anyhow::Result<()> {
    let parsed: Vec<(String, String)> = grants
        .iter()
        .map(|g| {
            g.split_once(':')
                .map(|(v, d)| (v.to_string(), d.to_string()))
                .ok_or_else(|| anyhow::anyhow!("grant {g:?} must be `verb:domain` (e.g. `read:*`)"))
        })
        .collect::<anyhow::Result<_>>()?;
    let token = match token {
        Some(token) => token,
        None => {
            let addr = match addr {
                Some(a) => a,
                None => crate::edge_call::daemon_addr(global)?,
            };
            stored_connection(&addr)?.token
        }
    };
    println!("{}", fq_edge::attenuate(&token, &parsed)?);
    Ok(())
}

#[cfg(test)]
mod tests;
