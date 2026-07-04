//! The op-boundary gate (M2 slice 5): access control at the named layer.
//!
//! [`GatedRepository`] wraps the named operations of a [`Repository`] and
//! evaluates every call under **belt-and-braces** enforcement (see the
//! [access-control guide](../../../docs/guide/access-control.md)):
//!
//! 1. **verify** the caller's token (signature chain, principal extraction),
//!    and re-validate the principal id's shape — the dot-free rule protects
//!    the `system.agents.<id>` own-scope boundary, so the gate never trusts
//!    the id embedded in a token;
//! 2. the token must **permit** the operation (TTL + the bearer's own
//!    attenuation — [`crate::VerifiedToken::permits`]);
//! 3. the **live projection** must authorize it
//!    ([`crate::SqliteGrantLog::can`]) — which is why revocation takes effect
//!    immediately, with no token-lifetime window.
//!
//! The gate is also where grant management gets its **up-front rejection**
//! (claim A4): a delegation is refused unless the delegator holds a live
//! `Grant` whose scope covers — and verbs contain — what is being delegated.
//! (The log beneath would tolerate such garbage inertly; the gate turns it
//! into a typed [`StoreError::Denied`] instead.) Revocation is allowed to the
//! **operator** always, and to an agent only for grants **it issued**.
//!
//! The raw [`Repository`], [`crate::ContentStore`], and [`SqliteGrantLog`]
//! remain preserved internal APIs for trusted in-process callers (the CLI's
//! operator paths, the collector, the audit); everything crossing a trust
//! boundary goes through this gate.

use std::collections::BTreeSet;
use std::time::SystemTime;

use crate::grant_log::SqliteGrantLog;
use crate::grants::{GrantId, Grantor, Principal, Scope, Verb};
use crate::tokens::TokenVerifier;
use crate::{BlockStore, Cid, NameIndex, Repository, Result, StoreError};

/// A [`Repository`] whose named operations are authorization-gated. Every
/// method takes the caller's token; the operation runs only if the token is
/// valid, permits the op, and the live projection authorizes it.
pub struct GatedRepository<C, N> {
    repo: Repository<C, N>,
    grants: SqliteGrantLog,
    verifier: TokenVerifier,
}

impl<C: BlockStore, N: NameIndex> GatedRepository<C, N> {
    /// Gate `repo` behind `grants` (the log + projection) and `verifier`.
    pub fn new(repo: Repository<C, N>, grants: SqliteGrantLog, verifier: TokenVerifier) -> Self {
        Self {
            repo,
            grants,
            verifier,
        }
    }

    /// Store `content` under `name` (requires `write` on `name`).
    pub async fn put(&self, token: &str, name: &str, content: &[u8]) -> Result<Cid> {
        self.authorize(token, Verb::Write, name).await?;
        self.repo.put(name, content).await
    }

    /// Bind `name` to an already-stored object (requires `write` on `name`).
    pub async fn bind(&self, token: &str, name: &str, cid: &Cid) -> Result<()> {
        self.authorize(token, Verb::Write, name).await?;
        self.repo.bind(name, cid).await
    }

    /// Read the content bound to `name` (requires `read` on `name`).
    pub async fn get(&self, token: &str, name: &str) -> Result<Vec<u8>> {
        self.authorize(token, Verb::Read, name).await?;
        self.repo.get(name).await
    }

    /// Read a byte range of `name` (requires `read` on `name`).
    pub async fn get_range(
        &self,
        token: &str,
        name: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        self.authorize(token, Verb::Read, name).await?;
        self.repo.get_range(name, offset, len).await
    }

    /// The current CID for `name` (requires `read` on `name`).
    pub async fn resolve(&self, token: &str, name: &str) -> Result<Option<Cid>> {
        self.authorize(token, Verb::Read, name).await?;
        self.repo.resolve(name).await
    }

    /// `name`'s version history (requires `read` on `name`).
    pub async fn history(&self, token: &str, name: &str) -> Result<Vec<Cid>> {
        self.authorize(token, Verb::Read, name).await?;
        self.repo.history(name).await
    }

    /// Unbind `name` (requires `delete` on `name`).
    pub async fn unbind(&self, token: &str, name: &str) -> Result<()> {
        self.authorize(token, Verb::Delete, name).await?;
        self.repo.unbind(name).await
    }

    /// List names under `prefix` (requires `list` on the namespace). Listing
    /// *everything* (an empty prefix) is an operator affordance, not a
    /// grantable one — no scope can cover the root — so the gate refuses it
    /// for token callers.
    pub async fn list(&self, token: &str, prefix: &str) -> Result<Vec<String>> {
        if prefix.is_empty() {
            return Err(StoreError::Denied(
                "listing all names requires the operator; supply a namespace prefix".into(),
            ));
        }
        self.authorize(token, Verb::List, prefix).await?;
        self.repo.list(prefix).await
    }

    /// Delegate: issue a grant of `verbs` over `scope` to `grantee`, as the
    /// token's principal. Refused (A4) unless the delegator holds a live
    /// `Grant` whose scope **covers** `scope` and whose verbs **contain**
    /// `verbs` — checked against the projection, and bounded by the token's
    /// own attenuation. The grantee id is validated like any principal id.
    pub async fn grant(
        &self,
        token: &str,
        grantee: &Principal,
        verbs: &BTreeSet<Verb>,
        scope: &Scope,
    ) -> Result<GrantId> {
        let scope_root = scope_root(scope);
        let principal = self.identify(token, Verb::Grant, scope_root).await?;
        if !grantee.has_valid_id() {
            return Err(StoreError::Token(
                "grantee id is not a valid agent id".into(),
            ));
        }
        let authority = self
            .grants
            .live_grants_for(&principal)
            .await?
            .into_iter()
            .any(|g| {
                g.verbs.contains(&Verb::Grant)
                    && g.verbs.is_superset(verbs)
                    && g.scope.covers_scope(scope)
            });
        if !authority {
            let Principal::Agent(id) = &principal;
            return Err(StoreError::Denied(format!(
                "{id} holds no live grant covering the delegation (grant verb, superset verbs, covering scope)"
            )));
        }
        let Principal::Agent(id) = &principal;
        self.grants
            .append_granted(&Grantor::Agent(id.clone()), grantee, verbs, scope)
            .await
    }

    /// Revoke `grant_id` as the token's principal. An agent may revoke only
    /// grants **it issued**; anything else is the operator's call
    /// ([`operator_revoke`](Self::operator_revoke)).
    pub async fn revoke(&self, token: &str, grant_id: GrantId) -> Result<()> {
        // Revocation targets a grant, not a name: identity + token bounds on
        // the grant verb apply, with the issuing check as the authority.
        let verified = self.verifier.verify(token)?;
        let principal = verified.principal().clone();
        if !principal.has_valid_id() {
            return Err(StoreError::Token(
                "principal id is not a valid agent id".into(),
            ));
        }
        let Principal::Agent(id) = &principal;
        match self.grants.grantor_of(grant_id).await? {
            Some(Grantor::Agent(issuer)) if issuer == *id => {
                self.grants.append_revoked(grant_id).await
            }
            Some(_) => Err(StoreError::Denied(format!(
                "{id} did not issue grant {grant_id}; only its issuer or the operator may revoke it"
            ))),
            None => Err(StoreError::Denied(format!(
                "grant {grant_id} does not exist"
            ))),
        }
    }

    /// Operator grant: root authority, no token — the store owner acting
    /// locally (the CLI). Trust is possession of the process/store, exactly
    /// like every other ungated internal API.
    pub async fn operator_grant(
        &self,
        grantee: &Principal,
        verbs: &BTreeSet<Verb>,
        scope: &Scope,
    ) -> Result<GrantId> {
        if !grantee.has_valid_id() {
            return Err(StoreError::Token(
                "grantee id is not a valid agent id".into(),
            ));
        }
        self.grants
            .append_granted(&Grantor::Operator, grantee, verbs, scope)
            .await
    }

    /// Operator revocation: root authority, revokes any grant.
    pub async fn operator_revoke(&self, grant_id: GrantId) -> Result<()> {
        if self.grants.grantor_of(grant_id).await?.is_none() {
            return Err(StoreError::Denied(format!(
                "grant {grant_id} does not exist"
            )));
        }
        self.grants.append_revoked(grant_id).await
    }

    /// The grants log + projection (trusted, ungated — for the operator CLI
    /// and token minting).
    pub fn grants(&self) -> &SqliteGrantLog {
        &self.grants
    }

    /// The underlying repository (trusted, ungated — the preserved internal
    /// API for in-process callers like the collector and the audit).
    pub fn repository(&self) -> &Repository<C, N> {
        &self.repo
    }

    /// The full gate for a named operation: verify the token, validate the
    /// principal id, check the token permits the op, check the projection
    /// authorizes it. Returns the acting principal.
    async fn authorize(&self, token: &str, verb: Verb, resource: &str) -> Result<Principal> {
        let principal = self.identify(token, verb, resource).await?;
        if !self.grants.can(&principal, verb, resource).await? {
            let Principal::Agent(id) = &principal;
            return Err(StoreError::Denied(format!(
                "{id} may not {verb:?} {resource}"
            )));
        }
        Ok(principal)
    }

    /// The identity + token-bounds half of the gate (no projection check):
    /// signature, principal-id shape, TTL, and the bearer's attenuation.
    async fn identify(&self, token: &str, verb: Verb, resource: &str) -> Result<Principal> {
        let verified = self.verifier.verify(token)?;
        let principal = verified.principal().clone();
        if !principal.has_valid_id() {
            return Err(StoreError::Token(
                "principal id is not a valid agent id".into(),
            ));
        }
        if !verified.permits(verb, resource, SystemTime::now()) {
            let Principal::Agent(id) = &principal;
            return Err(StoreError::Denied(format!(
                "{id}'s token does not permit {verb:?} on {resource} (expired or attenuated)"
            )));
        }
        Ok(principal)
    }
}

/// The name a scope is anchored at — the resource the token's attenuation is
/// checked against for delegation.
fn scope_root(scope: &Scope) -> &str {
    match scope {
        Scope::Name(name) => name,
        Scope::Namespace(ns) => ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FilesystemStore;
    use crate::tokens::{TokenMinter, generate_keypair};
    use crate::{SqliteNameIndex, verify};
    use std::time::Duration;

    struct Fixture {
        _dir: tempfile::TempDir,
        gate: GatedRepository<FilesystemStore, SqliteNameIndex>,
        minter: TokenMinter,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let cas = dir.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let store = FilesystemStore::with_params(cas, crate::fs::ChunkParams::small());
        let index = SqliteNameIndex::open(dir.path().join("index.db"))
            .await
            .unwrap();
        let repo = Repository::new(store, index);
        let grants = SqliteGrantLog::open(dir.path().join("grants.db"))
            .await
            .unwrap();
        let (private, public) = generate_keypair();
        let minter =
            TokenMinter::from_private_key_hex(&private, Duration::from_secs(3600)).unwrap();
        let verifier = TokenVerifier::from_public_key_hex(&public).unwrap();
        Fixture {
            _dir: dir,
            gate: GatedRepository::new(repo, grants, verifier),
            minter,
        }
    }

    fn alice() -> Principal {
        Principal::Agent("alice".into())
    }

    fn bob() -> Principal {
        Principal::Agent("bob".into())
    }

    async fn token_for(f: &Fixture, principal: &Principal) -> String {
        f.minter.mint_for(f.gate.grants(), principal).await.unwrap()
    }

    #[tokio::test]
    async fn default_deny_and_own_scope_end_to_end() {
        let f = fixture().await;
        let token = token_for(&f, &alice()).await;

        // A1: nothing granted — cross-agent names deny, own scope allows.
        assert!(matches!(
            f.gate.put(&token, "research.notes", b"hi").await,
            Err(StoreError::Denied(_))
        ));
        assert!(matches!(
            f.gate.get(&token, "docs.readme").await,
            Err(StoreError::Denied(_))
        ));
        f.gate
            .put(&token, "system.agents.alice.files.notes", b"mine")
            .await
            .unwrap();
        assert_eq!(
            f.gate
                .get(&token, "system.agents.alice.files.notes")
                .await
                .unwrap(),
            b"mine"
        );
        // …and alice cannot read bob's own scope.
        assert!(matches!(
            f.gate.get(&token, "system.agents.bob.files.x").await,
            Err(StoreError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn bad_credentials_are_token_errors_not_denials() {
        let f = fixture().await;
        assert!(matches!(
            f.gate.get("not-a-token", "docs.readme").await,
            Err(StoreError::Token(_))
        ));
    }

    #[tokio::test]
    async fn a_dotted_principal_id_is_rejected_at_the_boundary() {
        let f = fixture().await;
        // A doctored token claiming the "agent" `alice.files`: its own-scope
        // would nest inside alice's subtree. The mint API happily signs it —
        // the GATE must be the one to refuse.
        let forged = Principal::Agent("alice.files".into());
        let token = f.minter.mint(&forged, &[]).unwrap();
        assert!(matches!(
            f.gate.get(&token, "system.agents.alice.files.secret").await,
            Err(StoreError::Token(_))
        ));
    }

    #[tokio::test]
    async fn grant_then_operate_then_revoke_is_immediate() {
        let f = fixture().await;
        let token = token_for(&f, &alice()).await;

        let id = f
            .gate
            .operator_grant(
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Write]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        f.gate.put(&token, "research.notes", b"data").await.unwrap();
        assert_eq!(f.gate.get(&token, "research.notes").await.unwrap(), b"data");

        // A3 end-to-end: the very next call after revocation denies — same
        // token, no re-mint, no TTL involved.
        f.gate.operator_revoke(id).await.unwrap();
        assert!(matches!(
            f.gate.get(&token, "research.notes").await,
            Err(StoreError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn delegation_end_to_end_with_upfront_rejection() {
        let f = fixture().await;
        f.gate
            .operator_grant(
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Write, Verb::Grant]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        let alice_token = token_for(&f, &alice()).await;

        // A4: alice may delegate a subset within her scope…
        let delegated = f
            .gate
            .grant(
                &alice_token,
                &bob(),
                &BTreeSet::from([Verb::Read]),
                &Scope::Namespace("research.papers".into()),
            )
            .await
            .unwrap();
        // …after which bob (with a fresh token) can read there.
        f.gate
            .put(&alice_token, "research.papers.doc1", b"paper")
            .await
            .unwrap();
        let bob_token = token_for(&f, &bob()).await;
        assert_eq!(
            f.gate
                .get(&bob_token, "research.papers.doc1")
                .await
                .unwrap(),
            b"paper"
        );

        // Wider scope than alice holds: refused up front.
        assert!(matches!(
            f.gate
                .grant(
                    &alice_token,
                    &bob(),
                    &BTreeSet::from([Verb::Read]),
                    &Scope::Namespace("docs".into()),
                )
                .await,
            Err(StoreError::Denied(_))
        ));
        // Verbs alice does not hold: refused.
        assert!(matches!(
            f.gate
                .grant(
                    &alice_token,
                    &bob(),
                    &BTreeSet::from([Verb::Delete]),
                    &Scope::Namespace("research.papers".into()),
                )
                .await,
            Err(StoreError::Denied(_))
        ));
        // bob (no Grant verb) cannot delegate at all.
        assert!(matches!(
            f.gate
                .grant(
                    &bob_token,
                    &alice(),
                    &BTreeSet::from([Verb::Read]),
                    &Scope::Namespace("research.papers".into()),
                )
                .await,
            Err(StoreError::Denied(_))
        ));

        // Revocation rules: bob didn't issue alice's delegation — denied;
        // alice (the issuer) may revoke it, and bob loses access immediately.
        assert!(matches!(
            f.gate.revoke(&bob_token, delegated).await,
            Err(StoreError::Denied(_))
        ));
        f.gate.revoke(&alice_token, delegated).await.unwrap();
        assert!(matches!(
            f.gate.get(&bob_token, "research.papers.doc1").await,
            Err(StoreError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn a_name_scoped_grant_cannot_delegate_a_namespace() {
        let f = fixture().await;
        // alice holds Grant over exactly ONE NAME — not the subtree.
        f.gate
            .operator_grant(
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Grant]),
                &Scope::Name("research.papers".into()),
            )
            .await
            .unwrap();
        let token = token_for(&f, &alice()).await;
        // Delegating the NAMESPACE anchored at the same string must fail:
        // Scope::covers_scope, not string equality, is the rule.
        assert!(matches!(
            f.gate
                .grant(
                    &token,
                    &bob(),
                    &BTreeSet::from([Verb::Read]),
                    &Scope::Namespace("research.papers".into()),
                )
                .await,
            Err(StoreError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn an_attenuated_token_bounds_operations_and_delegation() {
        let f = fixture().await;
        f.gate
            .operator_grant(
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Write, Verb::Grant]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        let token = token_for(&f, &alice()).await;
        // The bearer attenuates offline, holding only the public key.
        let verifier = TokenVerifier::from_public_key_hex(&f.minter.public_key_hex()).unwrap();
        let narrowed = verifier
            .attenuate(
                &token,
                Some(&Scope::Namespace("research.papers".into())),
                Some(&BTreeSet::from([Verb::Read])),
            )
            .unwrap();

        f.gate
            .put(&token, "research.papers.doc1", b"paper")
            .await
            .unwrap();
        // The narrowed token reads inside its attenuation…
        assert_eq!(
            f.gate.get(&narrowed, "research.papers.doc1").await.unwrap(),
            b"paper"
        );
        // …but cannot write (verb attenuated away), read outside the subtree,
        // or delegate (grant attenuated away) — even though the PROJECTION
        // would allow alice all of it.
        assert!(matches!(
            f.gate.put(&narrowed, "research.papers.doc2", b"x").await,
            Err(StoreError::Denied(_))
        ));
        assert!(matches!(
            f.gate.get(&narrowed, "research.other").await,
            Err(StoreError::Denied(_))
        ));
        assert!(matches!(
            f.gate
                .grant(
                    &narrowed,
                    &bob(),
                    &BTreeSet::from([Verb::Read]),
                    &Scope::Namespace("research.papers".into()),
                )
                .await,
            Err(StoreError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn list_requires_a_namespace_and_a_grant() {
        let f = fixture().await;
        f.gate
            .operator_grant(
                &alice(),
                &BTreeSet::from([Verb::Write, Verb::List]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        let token = token_for(&f, &alice()).await;
        f.gate.put(&token, "research.a", b"1").await.unwrap();
        f.gate.put(&token, "research.b", b"2").await.unwrap();

        assert_eq!(
            f.gate.list(&token, "research").await.unwrap(),
            vec!["research.a".to_string(), "research.b".to_string()]
        );
        assert!(matches!(
            f.gate.list(&token, "").await,
            Err(StoreError::Denied(_))
        ));
        assert!(matches!(
            f.gate.list(&token, "docs").await,
            Err(StoreError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn the_gated_store_stays_consistent_under_the_oracle() {
        let f = fixture().await;
        f.gate
            .operator_grant(
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Write, Verb::Delete]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        let token = token_for(&f, &alice()).await;
        f.gate.put(&token, "research.a", b"one").await.unwrap();
        f.gate.put(&token, "research.b", b"two").await.unwrap();
        f.gate.unbind(&token, "research.a").await.unwrap();
        // The storage invariants hold beneath the gate.
        verify::assert_clean(f.gate.repository().index(), f.gate.repository().content()).await;
    }
}
