//! Edge identity: the daemon's self-signed certificate, its biscuit
//! root keypair, and token minting/verification (ADR-0031 Appendix A).
//!
//! Tokens carry `(verb, domain)` **grant facts** plus a principal;
//! authorisation is a per-request biscuit check of the resolved
//! operation's required authority against those grants, with `"*"` as
//! the wildcard on either position. Scoped clients (the read-only
//! dashboard) come from offline attenuation of the admin token — no
//! daemon round-trip.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use biscuit_auth::datalog::RunLimits;
use biscuit_auth::macros::authorizer;
use biscuit_auth::{Algorithm, Biscuit, KeyPair, PrivateKey, PublicKey};

/// Biscuit's default datalog budget is ~1ms of wall time — small
/// enough that scheduler jitter under load fails valid tokens. Our
/// programs are tiny; give them real headroom and keep failure
/// closed.
fn run_limits() -> RunLimits {
    RunLimits {
        max_time: Duration::from_millis(250),
        ..RunLimits::default()
    }
}
use fq_ops::Authority;
use sha2::{Digest, Sha256};

/// Everything the daemon needs to terminate the edge: TLS material
/// and the token root. Provisioned once (`EdgeIdentity::provision`)
/// and loadable from disk thereafter.
pub struct EdgeIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub root: KeyPair,
}

impl EdgeIdentity {
    /// Mint a fresh identity: self-signed certificate + biscuit root
    /// keypair. The caller persists it with the admin token beside it
    /// ([`save_minted`](Self::save_minted)) and prints the certificate
    /// fingerprint exactly once.
    pub fn provision() -> anyhow::Result<Self> {
        let cert = rcgen::generate_simple_self_signed(vec!["fqd".to_string()])?;
        Ok(EdgeIdentity {
            cert_der: cert.cert.der().to_vec(),
            key_der: cert.key_pair.serialize_der(),
            root: KeyPair::new(),
        })
    }

    /// The certificate fingerprint clients pin: SHA-256 over the DER.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.cert_der)
    }

    /// Mint the all-authority admin token — the one a first run writes
    /// to [`ADMIN_TOKEN_FILE`]. Every narrower token is an offline
    /// attenuation of this one.
    pub fn mint_admin_token(&self) -> anyhow::Result<String> {
        self.mint_token("admin", &[("*", "*")])
    }

    /// Mint the admin token and write it to `<dir>/admin.token`,
    /// owner-only from the first byte and never over an existing file
    /// (the same `write_secret` discipline as the key material). Only
    /// [`save_minted`](Self::save_minted) calls this, *before* the
    /// certificate goes down — see there for why the order matters.
    /// The caller reports *where* the token is, never the token, so it
    /// stays out of journald, `docker logs` and every run log for the
    /// life of the file (<https://github.com/bricef/factor-q/issues/545>).
    /// One trailing newline, so `$(cat …)` and `read` both hand back
    /// the bare token.
    fn write_admin_token(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let token = self.mint_admin_token()?;
        let path = dir.join(ADMIN_TOKEN_FILE);
        write_secret(&path, format!("{token}\n").as_bytes())?;
        Ok(path)
    }

    /// Write the certificate fingerprint, lowercase hex, to
    /// `<dir>/fingerprint`. Public and derived from `cert.der`, so it is
    /// plainly overwritten; it exists so a script can pin the daemon
    /// (`fq connect --fingerprint`) without scraping its stdout.
    pub fn write_fingerprint(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let path = dir.join(FINGERPRINT_FILE);
        fs::write(&path, format!("{}\n", fingerprint_hex(self.fingerprint())))?;
        Ok(path)
    }

    /// Mint a token for `principal` with explicit `(verb, domain)`
    /// grants (`"*"` wildcards allowed on either position).
    pub fn mint_token(&self, principal: &str, grants: &[(&str, &str)]) -> anyhow::Result<String> {
        let mut builder = Biscuit::builder();
        builder = builder.fact(biscuit_auth::builder::fact(
            "principal",
            &[biscuit_auth::builder::string(principal)],
        ))?;
        for (verb, domain) in grants {
            builder = builder.fact(biscuit_auth::builder::fact(
                "grant",
                &[
                    biscuit_auth::builder::string(verb),
                    biscuit_auth::builder::string(domain),
                ],
            ))?;
        }
        let token = builder.build(&self.root)?;
        Ok(token.to_base64()?)
    }

    pub fn public_key(&self) -> PublicKey {
        self.root.public()
    }

    /// Load the identity persisted under `dir`, or provision a fresh
    /// one and persist it. The `bool` is `true` exactly when the
    /// identity was freshly minted — the run on which the admin token
    /// is written beside it ([`save_minted`](Self::save_minted)), and
    /// never again.
    pub fn load_or_provision(dir: &Path) -> anyhow::Result<(Self, bool)> {
        if let Some(identity) = Self::try_load(dir)? {
            return Ok((identity, false));
        }
        let identity = Self::provision()?;
        identity.save_minted(dir)?;
        Ok((identity, true))
    }

    /// Load the identity persisted under `dir`; failing that, **adopt**
    /// one persisted under `legacy` before falling back to minting a
    /// fresh one (#362 — the identity moved from the cache directory
    /// to the state directory, and an upgrade must not orphan the
    /// clients pinned to the identity already on disk).
    ///
    /// Adoption *copies*: the identity is re-`save`d at `dir`, and the
    /// legacy copy is left untouched. Three reasons, in order of
    /// weight. First, `save` re-applies the hardening at creation time
    /// — 0700 directory, 0600 `create_new` key files — whereas a
    /// rename would carry whatever bits the old directory happened to
    /// have. Second, a rename across filesystems fails outright
    /// (`EXDEV`), which is exactly the cache-on-tmpfs shape this move
    /// exists to fix. Third, deleting durable secret material as a
    /// side effect of an upgrade is a one-way door: leaving the old
    /// copy keeps a rollback to the previous binary working. The
    /// legacy read happens at most once — from the next start the new
    /// location wins.
    pub fn load_or_adopt(dir: &Path, legacy: &Path) -> anyhow::Result<(Self, IdentityOrigin)> {
        if let Some(identity) = Self::try_load(dir)? {
            return Ok((identity, IdentityOrigin::Loaded));
        }
        if let Some(identity) = Self::try_load(legacy)? {
            identity.save(dir)?;
            return Ok((identity, IdentityOrigin::Adopted));
        }
        let identity = Self::provision()?;
        identity.save_minted(dir)?;
        Ok((identity, IdentityOrigin::Minted))
    }

    /// Read the identity under `dir` if it holds a complete one,
    /// `Ok(None)` if it holds none at all, and an error if it holds a
    /// *partial* one.
    ///
    /// The certificate is the completeness marker ([`save`](Self::save)
    /// writes it last). A cert-less directory still holding private
    /// material — the key, the root, or the admin token minted under
    /// that root — is a partial identity: provisioning over it, or
    /// skipping past it to another location, would silently rotate the
    /// root and orphan every pinned client and every issued token.
    /// Fail closed; the operator restores the missing file or deletes
    /// the directory to rotate deliberately.
    fn try_load(dir: &Path) -> anyhow::Result<Option<Self>> {
        if dir.join(CERT_FILE).exists() {
            return Ok(Some(Self::load(dir)?));
        }
        for name in [KEY_FILE, ROOT_FILE, ADMIN_TOKEN_FILE] {
            if dir.join(name).exists() {
                anyhow::bail!(
                    "edge identity at {} is partial: {name} exists but {CERT_FILE} is \
                     missing; restore the missing file, or delete the directory to \
                     provision a fresh identity (this invalidates all issued tokens \
                     and pinned fingerprints)",
                    dir.display()
                );
            }
        }
        Ok(None)
    }

    /// Persist an existing identity under `dir` (created 0700 on unix
    /// if absent) — the adoption path (#362) and tests. Private
    /// material — the TLS key and the token root — is written 0600 on
    /// unix, and never over an existing file: permissions are only
    /// applied at creation, so overwriting could silently inherit
    /// looser bits. No admin token: one was minted when this identity
    /// was, and a copy of an identity is not a new root.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        self.save_private(dir)?;
        self.mark_complete(dir)
    }

    /// Persist a freshly minted identity under `dir` *with* its admin
    /// token, in the one order that makes a crash safe: the private
    /// material and the fingerprint, then `admin.token`, and only then
    /// `cert.der`, the completeness marker. A failure anywhere before
    /// that last write leaves a partial identity — which `try_load`
    /// refuses on the next start — rather
    /// than a complete identity with no token, which every later start
    /// would load silently and nothing would ever say so. Returns the
    /// token's path.
    pub fn save_minted(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        self.save_private(dir)?;
        let token_path = self.write_admin_token(dir)?;
        self.mark_complete(dir)?;
        Ok(token_path)
    }

    /// Everything but the certificate: the directory, the key, the
    /// root, the fingerprint.
    fn save_private(&self, dir: &Path) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(dir)?;
        write_secret(&dir.join(KEY_FILE), &self.key_der)?;
        write_secret(
            &dir.join(ROOT_FILE),
            self.root.private().to_bytes_hex().as_bytes(),
        )?;
        // Public, derived from the certificate.
        self.write_fingerprint(dir)?;
        Ok(())
    }

    /// The certificate — public, and written last so its presence marks
    /// a complete identity (`try_load` keys on it).
    fn mark_complete(&self, dir: &Path) -> anyhow::Result<()> {
        fs::write(dir.join(CERT_FILE), &self.cert_der)?;
        Ok(())
    }

    /// Load an identity previously [`save`](Self::save)d under `dir`.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let cert_der = fs::read(dir.join(CERT_FILE))?;
        let key_der = fs::read(dir.join(KEY_FILE))?;
        let root_hex = fs::read_to_string(dir.join(ROOT_FILE))?;
        let private = PrivateKey::from_bytes_hex(root_hex.trim(), Algorithm::Ed25519)
            .map_err(|e| anyhow::anyhow!("edge token root key: {e}"))?;
        Ok(EdgeIdentity {
            cert_der,
            key_der,
            root: KeyPair::from(&private),
        })
    }
}

/// Where the identity a daemon just started with came from. Only
/// [`Minted`](IdentityOrigin::Minted) is a new root — the other two
/// keep every pinned fingerprint and issued token valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityOrigin {
    /// Read from the configured location: the steady state.
    Loaded,
    /// Read from the legacy location and copied to the configured one.
    Adopted,
    /// Neither location held one, so a fresh identity was provisioned
    /// and its admin token written beside it.
    Minted,
}

const CERT_FILE: &str = "cert.der";
const KEY_FILE: &str = "key.der";
const ROOT_FILE: &str = "root.key";
/// The admin token, beside the identity: written once at first run by
/// [`EdgeIdentity::save_minted`], owner-only, never overwritten, never
/// printed. The operator (and every test) reads it from here.
pub const ADMIN_TOKEN_FILE: &str = "admin.token";
/// The certificate fingerprint as lowercase hex, beside the identity —
/// public, so a script can pass it to `fq connect --fingerprint`
/// instead of scraping the daemon's stdout.
pub const FINGERPRINT_FILE: &str = "fingerprint";

/// Write private key material with owner-only permissions from the
/// first byte — created 0600 rather than chmodded after, so there is
/// no world-readable window. Refuses an existing file: `mode` is only
/// honoured when `open(2)` creates the inode, so overwriting would
/// silently keep whatever (possibly looser) bits the old file had —
/// fail closed instead.
#[cfg(unix)]
fn write_secret(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow::anyhow!("refusing to write {}: {e}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("refusing to write {}: {e}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Narrow a token offline — no root key, no daemon round-trip
/// (ADR-0031 Appendix A: scoped clients come from offline attenuation
/// of a broader token). Appends a block whose check constrains the
/// operations the token authorises to the given `(verb, domain)`
/// grants, `"*"` wildcard allowed on either position. Attenuation
/// only ever narrows: the appended check must pass *in addition to*
/// the original grants, and chained attenuations authorise the
/// intersection. The principal stays the minter's — it is signed into
/// the authority block; relabelling is a token-lifecycle design, not
/// an attenuation.
pub fn attenuate(token: &str, grants: &[(String, String)]) -> anyhow::Result<String> {
    if grants.is_empty() {
        anyhow::bail!("attenuation needs at least one (verb, domain) grant to narrow to");
    }
    // The grant segments are spliced into datalog source: validate
    // hard so a hostile segment cannot smuggle syntax in.
    for (verb, domain) in grants {
        validate_grant_segment(verb)?;
        validate_grant_segment(domain)?;
    }
    let conditions: Vec<String> = grants
        .iter()
        .map(|(verb, domain)| {
            let v = if verb == "*" {
                "true".to_string()
            } else {
                format!("$ov == \"{verb}\"")
            };
            let d = if domain == "*" {
                "true".to_string()
            } else {
                format!("$od == \"{domain}\"")
            };
            format!("({v} && {d})")
        })
        .collect();
    let check = format!(
        "check if operation($ov, $od), ({})",
        conditions.join(" || ")
    );
    let token = biscuit_auth::UnverifiedBiscuit::from_base64(token)?;
    let block = biscuit_auth::builder::BlockBuilder::new().check(check.as_str())?;
    Ok(token.append(block)?.to_base64()?)
}

/// A grant segment is a snake_case word or the `"*"` wildcard —
/// anything else is refused before it reaches datalog source.
fn validate_grant_segment(segment: &str) -> anyhow::Result<()> {
    let word = !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if word || segment == "*" {
        Ok(())
    } else {
        anyhow::bail!("invalid grant segment {segment:?}: expected snake_case or \"*\"");
    }
}

/// SHA-256 fingerprint of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hasher.finalize().into()
}

/// The pin's textual form — lowercase hex, what a daemon prints at
/// first run and what a client stores, passes on a flag, or reads from
/// its environment. Every client hand-writes this pair otherwise, and
/// the parser has a sharp edge worth owning once (see below).
pub fn fingerprint_hex(fingerprint: [u8; 32]) -> String {
    fingerprint.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse a pin from its textual form.
pub fn parse_fingerprint_hex(hex: &str) -> anyhow::Result<[u8; 32]> {
    let hex = hex.trim();
    // Length is checked in bytes but sliced on char boundaries below —
    // reject non-ASCII first so a 64-byte multi-byte string errors
    // cleanly instead of panicking mid-codepoint.
    if !hex.is_ascii() {
        anyhow::bail!("fingerprint is not valid hex: {hex:?}");
    }
    if hex.len() != 64 {
        anyhow::bail!(
            "fingerprint must be 64 hex chars (SHA-256), got {} chars",
            hex.len()
        );
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| anyhow::anyhow!("fingerprint is not valid hex: {hex:?}"))?;
    }
    Ok(out)
}

/// A verified connection identity: the parsed token plus its
/// principal, checked once at connection establishment and consulted
/// per request.
pub struct VerifiedToken {
    token: Biscuit,
    pub principal: String,
}

/// Verify a presented token against the root public key and extract
/// its principal. Fails closed on any parse/signature problem.
pub fn verify_token(presented: &str, root: PublicKey) -> anyhow::Result<VerifiedToken> {
    let token = Biscuit::from_base64(presented, root)?;
    let mut az = authorizer!(
        r#"
        allow if true;
        "#
    )
    .build(&token)?;
    let principals: Vec<(String,)> =
        az.query_with_limits("data($p) <- principal($p)", run_limits())?;
    let principal = principals
        .first()
        .map(|(p,)| p.clone())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(VerifiedToken { token, principal })
}

impl VerifiedToken {
    /// Does this token's grant set cover every required authority?
    /// Each requirement must be matched by a grant whose verb and
    /// domain each equal the requirement or `"*"`.
    pub fn allows(&self, required: &[Authority]) -> bool {
        required.iter().all(|authority| {
            let verb = authority.verb.segment();
            let domain = authority.scope.segment();
            let mut az = match authorizer!(
                r#"
                operation({verb}, {domain});
                allow if operation($ov, $od), grant($v, $d),
                    ($v == "*" || $v == $ov),
                    ($d == "*" || $d == $od);
                "#,
                verb = verb,
                domain = domain,
            )
            .build(&self.token)
            {
                Ok(az) => az,
                Err(_) => return false,
            };
            az.authorize_with_limits(run_limits()).is_ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-byte string with a multi-byte codepoint passes the length
    /// gate; slicing must not panic mid-codepoint (ultrareview
    /// bug_001) — it errors like any other junk.
    #[test]
    fn fingerprint_hex_rejects_non_ascii_without_panicking() {
        let hostile = format!("a\u{e9}{}", "a".repeat(61));
        assert_eq!(hostile.len(), 64, "64 bytes, 63 chars — the trap input");
        let err = parse_fingerprint_hex(&hostile).unwrap_err();
        assert!(err.to_string().contains("not valid hex"), "{err}");
    }

    /// The pair round-trips, so a stored pin re-pins the same daemon.
    #[test]
    fn a_pin_round_trips_through_its_textual_form() {
        let pin = fingerprint(b"a certificate");
        assert_eq!(parse_fingerprint_hex(&fingerprint_hex(pin)).unwrap(), pin);
    }
}
