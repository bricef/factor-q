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
use std::path::Path;
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
    /// keypair. The caller persists the parts it wants and prints the
    /// admin token + certificate fingerprint exactly once.
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

    /// Mint the all-authority admin token, printed at first run. Every
    /// narrower token is an offline attenuation of this one.
    pub fn mint_admin_token(&self) -> anyhow::Result<String> {
        self.mint_token("admin", &[("*", "*")])
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
    /// identity was freshly minted — the caller prints the admin token
    /// on that run and never again.
    pub fn load_or_provision(dir: &Path) -> anyhow::Result<(Self, bool)> {
        if dir.join(CERT_FILE).exists() {
            return Ok((Self::load(dir)?, false));
        }
        // A cert-less directory still holding private material is a
        // partial identity — re-provisioning over it would silently
        // rotate the root, orphaning every pinned client and every
        // issued token. Fail closed; the operator restores the missing
        // file or deletes the directory to rotate deliberately.
        for name in [KEY_FILE, ROOT_FILE] {
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
        let identity = Self::provision()?;
        identity.save(dir)?;
        Ok((identity, true))
    }

    /// Persist the identity under `dir` (created 0700 on unix if
    /// absent). Private material — the TLS key and the token root —
    /// is written 0600 on unix, and never over an existing file:
    /// permissions are only applied at creation, so overwriting could
    /// silently inherit looser bits.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
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
        // The certificate is public; written last so its presence
        // marks a complete identity (`load_or_provision` keys on it).
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

const CERT_FILE: &str = "cert.der";
const KEY_FILE: &str = "key.der";
const ROOT_FILE: &str = "root.key";

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
