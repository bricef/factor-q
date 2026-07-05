//! Capability tokens (M2 access control) — biscuit mint, verify, attenuate.
//!
//! A token is portable, cryptographically-verifiable proof of **identity**
//! (the principal), **freshness** (a TTL check), a **rights snapshot** (the
//! principal's live grants at mint time, flattened to `right(verb, kind,
//! value)` facts), and any **attenuation** the bearer applied offline. Minting
//! reads the projection ([`crate::SqliteGrantLog::live_grants_for`]) with the
//! private key; verification needs only the public key (ADR-0023 F4).
//!
//! Two authorization semantics, deliberately distinct (see the access-control
//! guide, `docs/guide/access-control.md`, and the M2 plan's belt-and-braces
//! decision):
//!
//! - [`VerifiedToken::authorizes`] — the **offline / remote** semantic: the
//!   operation must be covered by a `right` embedded in the token (and pass
//!   every attenuation check and the TTL). This is what a future remote
//!   verifier (M5) uses; its staleness is bounded by the TTL.
//! - [`VerifiedToken::permits`] — the **in-process gate** semantic: only the
//!   TTL and the bearer's attenuation are enforced; *authority* comes from the
//!   live projection instead (`token.permits(...) && projection.can(...)`).
//!   This is why revocation is immediate in-process, and why own-scope
//!   operations work with an unattenuated token.
//!
//! Attenuation ([`TokenVerifier::attenuate`]) appends a block of checks — it
//! can only narrow, never widen (claim A2): checks are conjunctive, so every
//! ancestor's constraints keep applying. All caller-supplied strings enter
//! datalog as **parameters**, never by string formatting, so a hostile name or
//! agent id cannot inject datalog.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use biscuit_auth::builder::{Algorithm, AuthorizerBuilder, BiscuitBuilder, BlockBuilder, Term};
use biscuit_auth::datalog::RunLimits;
use biscuit_auth::{Biscuit, KeyPair, PrivateKey, PublicKey};

use crate::grant_log::{LiveGrant, SqliteGrantLog};
use crate::grants::{Principal, Scope, Verb};
use crate::{Result, StoreError};

/// The default token lifetime: 300 seconds. Deliberately short — in-process
/// enforcement never relies on expiry (the projection is live), so the TTL
/// only prices how long a *remote* verifier (M5) could honour a stale token.
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(300);

/// Generate a fresh Ed25519 keypair, hex-encoded as `(private, public)` —
/// backs the `fq-cas key generate` command. The private key belongs to the
/// minting store only; verifiers get the public key.
pub fn generate_keypair() -> (String, String) {
    let keypair = KeyPair::new();
    (
        keypair.private().to_bytes_hex(),
        keypair.public().to_bytes_hex(),
    )
}

fn token_err(context: &str, e: impl std::fmt::Display) -> StoreError {
    StoreError::Token(format!("{context}: {e}"))
}

/// Datalog evaluation limits for every authorizer. Biscuit's default
/// `max_time` is **1 ms**, which under load fails evaluations
/// nondeterministically (a timeout reads as "deny") — our token programs are
/// tiny and trusted-shaped, so a generous wall-clock bound with the default
/// fact/iteration caps is the safe posture.
fn run_limits() -> RunLimits {
    RunLimits {
        max_time: Duration::from_millis(100),
        ..RunLimits::default()
    }
}

fn verb_term(verb: Verb) -> Term {
    Term::Str(verb.as_str().to_string())
}

fn date_term(at: SystemTime) -> Term {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Term::Date(secs)
}

/// Mints tokens from the live projection, holding the store's private key.
pub struct TokenMinter {
    keypair: KeyPair,
    ttl: Duration,
}

impl TokenMinter {
    /// A minter from the hex-encoded Ed25519 private key (CLI arg
    /// `--biscuit-private-key` / env `FQ_BISCUIT_PRIVATE_KEY`) and a token
    /// `ttl` (callers pass [`DEFAULT_TOKEN_TTL`] for the default). The error
    /// deliberately does not echo the supplied key material.
    pub fn from_private_key_hex(private_key_hex: &str, ttl: Duration) -> Result<Self> {
        let private = PrivateKey::from_bytes_hex(private_key_hex, Algorithm::Ed25519)
            .map_err(|_| StoreError::Token("private key: malformed hex or wrong length".into()))?;
        Ok(Self {
            keypair: KeyPair::from(&private),
            ttl,
        })
    }

    /// The public key verifying this minter's tokens, hex-encoded.
    pub fn public_key_hex(&self) -> String {
        self.keypair.public().to_bytes_hex()
    }

    /// Mint a token for `principal` carrying its **current** live grants from
    /// the projection — so a mint after a revocation never carries the revoked
    /// authority (A3: new mints fail immediately).
    pub async fn mint_for(&self, log: &SqliteGrantLog, principal: &Principal) -> Result<String> {
        let grants = log.live_grants_for(principal).await?;
        self.mint(principal, &grants)
    }

    /// Mint a token for `principal` carrying exactly `grants` as its rights
    /// snapshot, expiring [`ttl`](Self::from_private_key_hex) from now.
    pub fn mint(&self, principal: &Principal, grants: &[LiveGrant]) -> Result<String> {
        let Principal::Agent(agent) = principal;
        let expiry = SystemTime::now() + self.ttl;

        let mut params = HashMap::new();
        params.insert("principal".to_string(), Term::Str(agent.clone()));
        params.insert("expiry".to_string(), date_term(expiry));
        let mut builder = BiscuitBuilder::new()
            .code_with_params(
                "principal({principal});
                 check if time($t), $t <= {expiry};",
                params,
                HashMap::new(),
            )
            .map_err(|e| token_err("mint", e))?;

        // Rights, flattened one fact per verb: right(verb, kind, value).
        for (i, grant) in grants.iter().enumerate() {
            let (kind, value) = (grant.scope.kind(), grant.scope.value());
            for verb in &grant.verbs {
                let mut params = HashMap::new();
                params.insert("verb".to_string(), verb_term(*verb));
                params.insert("kind".to_string(), Term::Str(kind.to_string()));
                params.insert("value".to_string(), Term::Str(value.to_string()));
                builder = builder
                    .code_with_params("right({verb}, {kind}, {value});", params, HashMap::new())
                    .map_err(|e| token_err(&format!("mint right {i}"), e))?;
            }
        }

        let token = builder
            .build(&self.keypair)
            .map_err(|e| token_err("mint sign", e))?;
        token.to_base64().map_err(|e| token_err("mint encode", e))
    }
}

/// Verifies and attenuates tokens, holding only the store's public key (CLI
/// arg `--biscuit-public-key` / env `FQ_BISCUIT_PUBLIC_KEY`).
pub struct TokenVerifier {
    root: PublicKey,
}

impl TokenVerifier {
    /// A verifier from the hex-encoded Ed25519 public key.
    pub fn from_public_key_hex(public_key_hex: &str) -> Result<Self> {
        let root = PublicKey::from_bytes_hex(public_key_hex, Algorithm::Ed25519)
            .map_err(|_| StoreError::Token("public key: malformed hex or wrong length".into()))?;
        Ok(Self { root })
    }

    /// Check the token's signature chain and extract its principal.
    /// [`StoreError::Token`] if unparseable, wrongly signed, or missing its
    /// principal fact. Expiry and attenuation are enforced per operation by
    /// [`VerifiedToken::permits`] / [`VerifiedToken::authorizes`].
    pub fn verify(&self, token_b64: &str) -> Result<VerifiedToken> {
        let token =
            Biscuit::from_base64(token_b64, self.root).map_err(|e| token_err("verify", e))?;
        let principals: Vec<(String,)> = AuthorizerBuilder::new()
            .build(&token)
            .map_err(|e| token_err("verify", e))?
            .query_with_limits("data($p) <- principal($p)", run_limits())
            .map_err(|e| token_err("verify principal", e))?;
        let [(agent,)] = principals.as_slice() else {
            return Err(StoreError::Token(
                "token carries no unique principal".into(),
            ));
        };
        Ok(VerifiedToken {
            token,
            principal: Principal::Agent(agent.clone()),
        })
    }

    /// Attenuate `token_b64` offline: append a block whose checks narrow the
    /// token to `scope` (when given) and `verbs` (when given). Checks are
    /// conjunctive with every existing block, so the result authorizes a
    /// subset of its parent, always (A2) — attenuating to a *wider* scope
    /// simply has no effect beyond the parent's bounds.
    pub fn attenuate(
        &self,
        token_b64: &str,
        scope: Option<&Scope>,
        verbs: Option<&BTreeSet<Verb>>,
    ) -> Result<String> {
        let token =
            Biscuit::from_base64(token_b64, self.root).map_err(|e| token_err("attenuate", e))?;
        let mut block = BlockBuilder::new();
        if let Some(scope) = scope {
            let mut params = HashMap::new();
            match scope {
                Scope::Name(name) => {
                    params.insert("name".to_string(), Term::Str(name.clone()));
                    block = block
                        .code_with_params(
                            "check if resource($r), $r == {name};",
                            params,
                            HashMap::new(),
                        )
                        .map_err(|e| token_err("attenuate scope", e))?;
                }
                Scope::Namespace(ns) => {
                    params.insert("ns".to_string(), Term::Str(ns.clone()));
                    params.insert("ns_dot".to_string(), Term::Str(format!("{ns}.")));
                    block = block
                        .code_with_params(
                            "check if resource($r), $r == {ns} || $r.starts_with({ns_dot});",
                            params,
                            HashMap::new(),
                        )
                        .map_err(|e| token_err("attenuate scope", e))?;
                }
            }
        }
        if let Some(verbs) = verbs {
            let set = Term::Set(verbs.iter().map(|v| verb_term(*v)).collect());
            let mut params = HashMap::new();
            params.insert("verbs".to_string(), set);
            block = block
                .code_with_params(
                    "check if operation($op), {verbs}.contains($op);",
                    params,
                    HashMap::new(),
                )
                .map_err(|e| token_err("attenuate verbs", e))?;
        }
        let attenuated = token
            .append(block)
            .map_err(|e| token_err("attenuate append", e))?;
        attenuated
            .to_base64()
            .map_err(|e| token_err("attenuate encode", e))
    }
}

/// A signature-checked token: the principal it identifies, plus per-operation
/// checks under the two semantics (see the module docs).
pub struct VerifiedToken {
    token: Biscuit,
    principal: Principal,
}

impl VerifiedToken {
    /// The principal this token identifies.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The **offline / remote** decision: does an embedded `right` cover
    /// `verb` on `resource`, with every attenuation check and the TTL passing
    /// at time `at`? Any failure — expired, attenuated away, no covering
    /// right — is a plain `false` (deny-by-default; the credential itself
    /// being invalid was already rejected by [`TokenVerifier::verify`]).
    pub fn authorizes(&self, verb: Verb, resource: &str, at: SystemTime) -> bool {
        self.evaluate(verb, resource, at, true)
    }

    /// The **in-process gate** decision: do the TTL and the bearer's
    /// attenuation permit `verb` on `resource` at `at`? Embedded rights are
    /// *not* required — under belt-and-braces enforcement the live projection
    /// supplies authority, so an unattenuated token permits everything and
    /// the projection decides (including own-scope operations).
    pub fn permits(&self, verb: Verb, resource: &str, at: SystemTime) -> bool {
        self.evaluate(verb, resource, at, false)
    }

    fn evaluate(&self, verb: Verb, resource: &str, at: SystemTime, require_right: bool) -> bool {
        let mut params = HashMap::new();
        params.insert("op".to_string(), verb_term(verb));
        params.insert("res".to_string(), Term::Str(resource.to_string()));
        params.insert("at".to_string(), date_term(at));
        let facts = "time({at});
             operation({op});
             resource({res});";
        let policies = if require_right {
            // A right covers the resource exactly (a name right) or by
            // segment-aware namespace prefix (equal, or extends past a `.`).
            "allow if operation($v), resource($r), right($v, \"name\", $r);
             allow if operation($v), resource($r), right($v, \"namespace\", $ns), $r == $ns;
             allow if operation($v), resource($r), right($v, \"namespace\", $ns), $r.starts_with($ns + \".\");
             deny if true;"
        } else {
            "allow if true;"
        };
        let Ok(builder) = AuthorizerBuilder::new().code_with_params(facts, params, HashMap::new())
        else {
            return false;
        };
        let Ok(builder) = builder.code(policies) else {
            return false;
        };
        match builder.build(&self.token) {
            Ok(mut authorizer) => authorizer.authorize_with_limits(run_limits()).is_ok(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::test_strategies::{AGENTS, RESOURCES, arb_scope, arb_verbs};
    use crate::grants::{GrantId, Grantor};
    use proptest::prelude::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn minter() -> (TokenMinter, TokenVerifier) {
        let (private, public) = generate_keypair();
        (
            TokenMinter::from_private_key_hex(&private, HOUR).unwrap(),
            TokenVerifier::from_public_key_hex(&public).unwrap(),
        )
    }

    fn alice() -> Principal {
        Principal::Agent("alice".into())
    }

    fn live(id: GrantId, verbs: &[Verb], scope: Scope) -> LiveGrant {
        LiveGrant {
            id,
            verbs: verbs.iter().copied().collect(),
            scope,
        }
    }

    fn now() -> SystemTime {
        SystemTime::now()
    }

    #[test]
    fn verify_round_trips_the_principal() {
        let (minter, verifier) = minter();
        let token = minter.mint(&alice(), &[]).unwrap();
        let verified = verifier.verify(&token).unwrap();
        assert_eq!(verified.principal(), &alice());
    }

    #[test]
    fn wrong_key_and_tampering_are_rejected() {
        let (minter, verifier) = minter();
        let (_, other_public) = generate_keypair();
        let stranger = TokenVerifier::from_public_key_hex(&other_public).unwrap();
        let token = minter.mint(&alice(), &[]).unwrap();
        assert!(matches!(stranger.verify(&token), Err(StoreError::Token(_))));

        // Flip a character mid-token: the signature chain must break.
        let mut tampered = token.clone().into_bytes();
        let mid = tampered.len() / 2;
        tampered[mid] = if tampered[mid] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(verifier.verify(&tampered).is_err());
    }

    #[test]
    fn rights_authorize_exactly_their_cover() {
        let (minter, verifier) = minter();
        let grants = [live(
            1,
            &[Verb::Read, Verb::Write],
            Scope::Namespace("research".into()),
        )];
        let token = minter.mint(&alice(), &grants).unwrap();
        let verified = verifier.verify(&token).unwrap();

        assert!(verified.authorizes(Verb::Read, "research.papers.doc1", now()));
        assert!(verified.authorizes(Verb::Write, "research", now()));
        assert!(!verified.authorizes(Verb::Delete, "research.papers.doc1", now())); // verb not granted
        assert!(!verified.authorizes(Verb::Read, "researchX", now())); // segment boundary
        assert!(!verified.authorizes(Verb::Read, "docs.readme", now())); // outside scope
    }

    #[test]
    fn an_empty_rights_token_authorizes_nothing_but_permits_everything() {
        let (minter, verifier) = minter();
        let token = minter.mint(&alice(), &[]).unwrap();
        let verified = verifier.verify(&token).unwrap();
        // Offline semantics: no rights, no authority (A1 at the token level).
        assert!(!verified.authorizes(Verb::Read, "research.papers.doc1", now()));
        // Gate semantics: identity + TTL only — the projection decides, so an
        // unattenuated token places no bounds of its own.
        assert!(verified.permits(Verb::Read, "research.papers.doc1", now()));
        assert!(verified.permits(Verb::Write, "system.agents.alice.files.x", now()));
    }

    #[test]
    fn the_ttl_expires_both_semantics() {
        let (minter, verifier) = minter();
        let token = minter
            .mint(
                &alice(),
                &[live(1, &[Verb::Read], Scope::Namespace("research".into()))],
            )
            .unwrap();
        let verified = verifier.verify(&token).unwrap();
        let fresh = now();
        let expired = now() + HOUR + Duration::from_secs(60);
        assert!(verified.authorizes(Verb::Read, "research.x", fresh));
        assert!(verified.permits(Verb::Read, "research.x", fresh));
        assert!(!verified.authorizes(Verb::Read, "research.x", expired));
        assert!(!verified.permits(Verb::Read, "research.x", expired));
    }

    #[test]
    fn attenuation_narrows_scope_and_verbs() {
        let (minter, verifier) = minter();
        let grants = [live(
            1,
            &[Verb::Read, Verb::Write],
            Scope::Namespace("research".into()),
        )];
        let token = minter.mint(&alice(), &grants).unwrap();

        // Narrow to the papers subtree, read-only.
        let narrowed = verifier
            .attenuate(
                &token,
                Some(&Scope::Namespace("research.papers".into())),
                Some(&BTreeSet::from([Verb::Read])),
            )
            .unwrap();
        let verified = verifier.verify(&narrowed).unwrap();

        assert!(verified.authorizes(Verb::Read, "research.papers.doc1", now()));
        assert!(!verified.authorizes(Verb::Write, "research.papers.doc1", now())); // verb attenuated away
        assert!(!verified.authorizes(Verb::Read, "research.other", now())); // outside attenuated scope
        // permits() honours the same attenuation (the gate's bound):
        assert!(verified.permits(Verb::Read, "research.papers.doc1", now()));
        assert!(!verified.permits(Verb::Write, "research.papers.doc1", now()));
        assert!(!verified.permits(Verb::Read, "docs.readme", now()));
    }

    #[test]
    fn attenuation_respects_the_namespace_segment_boundary() {
        // A token with rights over `research` (which covers `research.papersX`),
        // attenuated to the namespace `research.papers`, must NOT act on
        // `research.papersX` — the attenuation check is segment-aware, not a
        // raw prefix. This pins the boundary in both semantics (the parent's
        // rights would otherwise mask a missing `.` in the attenuation).
        let (minter, verifier) = minter();
        let grants = [live(1, &[Verb::Read], Scope::Namespace("research".into()))];
        let token = minter.mint(&alice(), &grants).unwrap();
        let narrowed = verifier
            .attenuate(
                &token,
                Some(&Scope::Namespace("research.papers".into())),
                None,
            )
            .unwrap();
        let verified = verifier.verify(&narrowed).unwrap();
        assert!(verified.authorizes(Verb::Read, "research.papers.doc1", now()));
        assert!(!verified.authorizes(Verb::Read, "research.papersX", now()));
        assert!(verified.permits(Verb::Read, "research.papers.doc1", now()));
        assert!(!verified.permits(Verb::Read, "research.papersX", now()));
    }

    #[test]
    fn attenuation_cannot_widen_beyond_the_parent() {
        let (minter, verifier) = minter();
        let grants = [live(
            1,
            &[Verb::Read],
            Scope::Namespace("research.papers".into()),
        )];
        let token = minter.mint(&alice(), &grants).unwrap();
        // "Attenuate" to a WIDER scope: the parent's rights still bound authorizes.
        let widened = verifier
            .attenuate(&token, Some(&Scope::Namespace("research".into())), None)
            .unwrap();
        let verified = verifier.verify(&widened).unwrap();
        assert!(verified.authorizes(Verb::Read, "research.papers.doc1", now()));
        assert!(
            !verified.authorizes(Verb::Read, "research.other", now()),
            "a wider attenuation must not create authority"
        );
    }

    #[test]
    fn mint_reflects_the_live_projection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let log = SqliteGrantLog::open(dir.path().join("grants.db"))
                .await
                .unwrap();
            let (minter, verifier) = minter();

            let id = log
                .append_granted(
                    &Grantor::Operator,
                    &alice(),
                    &BTreeSet::from([Verb::Read]),
                    &Scope::Namespace("research".into()),
                )
                .await
                .unwrap();
            let token = minter.mint_for(&log, &alice()).await.unwrap();
            let verified = verifier.verify(&token).unwrap();
            assert!(verified.authorizes(Verb::Read, "research.x", now()));

            // Revoke, then mint again: the new token carries no stale authority.
            log.append_revoked(id).await.unwrap();
            let token = minter.mint_for(&log, &alice()).await.unwrap();
            let verified = verifier.verify(&token).unwrap();
            assert!(
                !verified.authorizes(Verb::Read, "research.x", now()),
                "new mints fail immediately after revocation (A3)"
            );
        });
    }

    /// The authorized set over the shared query grid.
    fn authorized_set(verified: &VerifiedToken, at: SystemTime) -> Vec<bool> {
        let mut out = Vec::new();
        for verb in Verb::all() {
            for resource in RESOURCES {
                out.push(verified.authorizes(verb, resource, at));
            }
        }
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// The datalog encoding agrees with the domain semantics: an
        /// unattenuated token authorizes exactly what its minted grants cover.
        #[test]
        fn token_rights_match_domain_cover(
            grants in proptest::collection::vec((arb_verbs(), arb_scope()), 0..4),
        ) {
            let (minter, verifier) = minter();
            let grants: Vec<LiveGrant> = grants
                .into_iter()
                .enumerate()
                .map(|(i, (verbs, scope))| LiveGrant { id: i as GrantId, verbs, scope })
                .collect();
            let token = minter.mint(&alice(), &grants).unwrap();
            let verified = verifier.verify(&token).unwrap();
            let at = now();
            for verb in Verb::all() {
                for resource in RESOURCES {
                    let expected = grants
                        .iter()
                        .any(|g| g.verbs.contains(&verb) && g.scope.covers(resource));
                    prop_assert_eq!(
                        verified.authorizes(verb, resource, at),
                        expected,
                        "verb {:?} on {}", verb, resource
                    );
                }
            }
        }

        /// A2 — attenuation never widens: whatever block we append, the
        /// authorized set is a subset of the parent's (and the same for the
        /// gate's permits semantics).
        #[test]
        fn attenuation_never_widens(
            grants in proptest::collection::vec((arb_verbs(), arb_scope()), 0..3),
            att_scope in proptest::option::of(arb_scope()),
            att_verbs in proptest::option::of(arb_verbs()),
            agent in proptest::sample::select(AGENTS),
        ) {
            let (minter, verifier) = minter();
            let grants: Vec<LiveGrant> = grants
                .into_iter()
                .enumerate()
                .map(|(i, (verbs, scope))| LiveGrant { id: i as GrantId, verbs, scope })
                .collect();
            let principal = Principal::Agent(agent.to_string());
            let token = minter.mint(&principal, &grants).unwrap();
            let attenuated = verifier
                .attenuate(&token, att_scope.as_ref(), att_verbs.as_ref())
                .unwrap();
            let parent = verifier.verify(&token).unwrap();
            let child = verifier.verify(&attenuated).unwrap();
            let at = now();
            for (p, c) in authorized_set(&parent, at)
                .into_iter()
                .zip(authorized_set(&child, at))
            {
                prop_assert!(!c || p, "attenuation widened the authorized set");
            }
            // permits: the child's permitted set is likewise a subset.
            for verb in Verb::all() {
                for resource in RESOURCES {
                    let p = parent.permits(verb, resource, at);
                    let c = child.permits(verb, resource, at);
                    prop_assert!(!c || p, "attenuation widened the permitted set");
                }
            }
        }
    }
}
