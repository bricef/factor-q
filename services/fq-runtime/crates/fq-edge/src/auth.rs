//! Edge identity: the daemon's self-signed certificate, its biscuit
//! root keypair, and token minting/verification (ADR-0031 Appendix A).
//!
//! Tokens carry `(verb, domain)` **grant facts** plus a principal;
//! authorisation is a per-request biscuit check of the resolved
//! operation's required authority against those grants, with `"*"` as
//! the wildcard on either position. Scoped clients (the read-only
//! dashboard) come from offline attenuation of the admin token — no
//! daemon round-trip.

use std::time::Duration;

use biscuit_auth::datalog::RunLimits;
use biscuit_auth::macros::authorizer;
use biscuit_auth::{Biscuit, KeyPair, PublicKey};

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
