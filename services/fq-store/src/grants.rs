//! Access control (M2): the grants domain model and its reference semantics.
//!
//! This module is the **executable specification** of authorization — the
//! vocabulary (principals, verbs, scopes, grant events) and a deliberately
//! naive, obviously-correct reference model ([`GrantModel`]) that answers
//! `can(principal, verb, resource)`. The design is ADR-0023 F4 (event-sourced
//! grant claims); the claims it must satisfy are A1–A6 in the
//! [M2 plan](https://github.com/bricef/factor-q) (`docs/plans/active/`,
//! 2026-07-03): default-deny (A1), revocation wins (A3), delegation is
//! grant-gated (A4). Later milestones' machinery — the grant projection, the
//! biscuit token gate — must agree with this model; the property tests here
//! prove the model itself, and differential tests elsewhere prove the
//! implementations against it.
//!
//! Semantics pinned by this model:
//!
//! - **Default-deny.** An operation is allowed only by a live covering grant —
//!   except inside the principal's **own scope** (`system.agents.<id>` and
//!   below), which needs no grant.
//! - **Liveness is evaluated at query time.** A grant is live if it is not
//!   revoked and its authority chain still stands: an operator grant is
//!   root-valid; an agent-issued grant (a delegation) is live only while some
//!   **earlier, still-live** grant gives the grantor `Grant` over a covering
//!   scope and a superset of the verbs. Revoking an upstream grant therefore
//!   kills the whole delegated subtree — revocation wins, transitively.
//! - **Attenuation at delegation.** A delegation confers at most what its
//!   grantor holds (scope ⊆, verbs ⊆); anything wider is simply **inert**.
//! - **The log may contain garbage; garbage confers nothing.** `apply` is
//!   total and deterministic (a projection must never diverge on replay):
//!   an unauthorized delegation, a duplicate grant id (first wins), or a
//!   revocation of an unknown id are all tolerated — the API gate (M2 slice 5)
//!   rejects them up front, but nothing relies on that for safety.

use std::collections::{BTreeSet, HashMap};

/// An access-control subject. Extensible by design (ADR-0023); v1 implements
/// agents only.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Principal {
    /// An agent, by id. Own scope: `system.agents.<id>` and below.
    Agent(String),
}

impl Principal {
    /// The namespace this principal owns outright (no grant needed).
    fn own_namespace(&self) -> String {
        match self {
            Principal::Agent(id) => format!("system.agents.{id}"),
        }
    }
}

/// Who issued a grant: the store operator (root authority — the local owner
/// acting via the CLI/service), or an agent delegating within its own
/// authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grantor {
    /// The store owner. Operator grants are root-valid (no supporting chain).
    Operator,
    /// A delegating agent — valid only while backed by a live `Grant` (A4).
    Agent(String),
}

/// The operation verbs a grant can confer (ADR-0023 F4). `Grant` is the
/// delegation verb: holding it (over a scope) is what authorizes issuing
/// further grants within that scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verb {
    Read,
    Write,
    Delete,
    List,
    Grant,
}

impl Verb {
    /// Every verb — the widest possible grant.
    pub fn all() -> BTreeSet<Verb> {
        BTreeSet::from([
            Verb::Read,
            Verb::Write,
            Verb::Delete,
            Verb::List,
            Verb::Grant,
        ])
    }
}

/// What a grant covers: one exact name, or a whole namespace subtree.
/// Namespace matching is segment-aware, exactly like [`crate::NameIndex::list`]:
/// `Namespace("research.papers")` covers `research.papers` and
/// `research.papers.<anything>`, and does **not** cover `research.papersX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Exactly this name.
    Name(String),
    /// This name and every dotted descendant.
    Namespace(String),
}

impl Scope {
    /// Whether `resource` (a dotted name) falls inside this scope.
    pub fn covers(&self, resource: &str) -> bool {
        match self {
            Scope::Name(name) => name == resource,
            Scope::Namespace(ns) => namespace_covers(ns, resource),
        }
    }

    /// Whether every resource in `other` also falls inside `self` — the
    /// subset relation delegation attenuation (and token attenuation, A2)
    /// is checked against.
    pub fn covers_scope(&self, other: &Scope) -> bool {
        match (self, other) {
            (_, Scope::Name(name)) => self.covers(name),
            // A namespace is only covered by a namespace at/above it: a
            // Name scope never covers the (infinite) subtree.
            (Scope::Name(_), Scope::Namespace(_)) => false,
            (Scope::Namespace(ns), Scope::Namespace(other_ns)) => namespace_covers(ns, other_ns),
        }
    }
}

/// Segment-aware namespace containment: `ns` covers `name` iff equal, or
/// `name` starts with `ns` followed by a `.` segment boundary.
fn namespace_covers(ns: &str, name: &str) -> bool {
    name == ns
        || (name.len() > ns.len() && name.starts_with(ns) && name.as_bytes()[ns.len()] == b'.')
}

/// A grant's identity — assigned by the event log (M2 slice 2); unique per
/// store. Revocations reference it, and delegation chains order by it.
pub type GrantId = u64;

/// A grant-domain event. This is the domain vocabulary; the wire schemas
/// (envelopes, `factor-q/granted@1`-style ids, NATS subjects) wrap it in M2
/// slice 2. A *delegation* is a `Granted` whose grantor is an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantEvent {
    /// Confer `verbs` over `scope` on `grantee`.
    Granted {
        id: GrantId,
        grantor: Grantor,
        grantee: Principal,
        verbs: BTreeSet<Verb>,
        scope: Scope,
    },
    /// Withdraw the grant with `id` (and, transitively, every delegation
    /// standing on it — see the module docs).
    Revoked { id: GrantId },
}

/// One applied grant, as the model stores it.
#[derive(Debug, Clone)]
struct GrantRow {
    grantor: Grantor,
    grantee: Principal,
    verbs: BTreeSet<Verb>,
    scope: Scope,
}

/// The reference authorization model — the naive, obviously-correct answer to
/// `can()`. Not an efficient implementation (liveness re-walks delegation
/// chains per query); its job is to be *right*, so the projection and the
/// token gate can be tested against it.
#[derive(Debug, Clone, Default)]
pub struct GrantModel {
    grants: HashMap<GrantId, GrantRow>,
    revoked: BTreeSet<GrantId>,
}

impl GrantModel {
    /// An empty model: nothing is granted, everything (outside own scopes)
    /// denied.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event, in log order. Total and deterministic: a duplicate
    /// grant id is ignored (first wins), a revocation is recorded whether or
    /// not the id is (yet) known — a revoked id can never confer authority,
    /// regardless of event order.
    pub fn apply(&mut self, event: &GrantEvent) {
        match event {
            GrantEvent::Granted {
                id,
                grantor,
                grantee,
                verbs,
                scope,
            } => {
                self.grants.entry(*id).or_insert_with(|| GrantRow {
                    grantor: grantor.clone(),
                    grantee: grantee.clone(),
                    verbs: verbs.clone(),
                    scope: scope.clone(),
                });
            }
            GrantEvent::Revoked { id } => {
                self.revoked.insert(*id);
            }
        }
    }

    /// Replay a whole log from scratch.
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a GrantEvent>) -> Self {
        let mut model = Self::new();
        for event in events {
            model.apply(event);
        }
        model
    }

    /// The authorization decision: may `principal` perform `verb` on the
    /// dotted name `resource`? Default-deny; own scope always allowed.
    pub fn can(&self, principal: &Principal, verb: Verb, resource: &str) -> bool {
        if namespace_covers(&principal.own_namespace(), resource) {
            return true;
        }
        self.grants.iter().any(|(id, grant)| {
            grant.grantee == *principal
                && grant.verbs.contains(&verb)
                && grant.scope.covers(resource)
                && self.is_live(*id)
        })
    }

    /// Whether the grant `id` currently confers authority: present, not
    /// revoked, and — for a delegation — still standing on a live supporting
    /// grant (which must be *earlier*, so chains are well-founded).
    fn is_live(&self, id: GrantId) -> bool {
        if self.revoked.contains(&id) {
            return false;
        }
        let Some(grant) = self.grants.get(&id) else {
            return false;
        };
        match &grant.grantor {
            Grantor::Operator => true,
            Grantor::Agent(agent) => {
                let delegator = Principal::Agent(agent.clone());
                self.grants.iter().any(|(sup_id, sup)| {
                    *sup_id < id
                        && sup.grantee == delegator
                        && sup.verbs.contains(&Verb::Grant)
                        && sup.verbs.is_superset(&grant.verbs)
                        && sup.scope.covers_scope(&grant.scope)
                        && self.is_live(*sup_id)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn agent(id: &str) -> Principal {
        Principal::Agent(id.into())
    }

    fn rw() -> BTreeSet<Verb> {
        BTreeSet::from([Verb::Read, Verb::Write])
    }

    fn grant(
        id: GrantId,
        grantor: Grantor,
        grantee: &str,
        verbs: BTreeSet<Verb>,
        scope: Scope,
    ) -> GrantEvent {
        GrantEvent::Granted {
            id,
            grantor,
            grantee: agent(grantee),
            verbs,
            scope,
        }
    }

    // ---- A1: default-deny + own scope ----

    #[test]
    fn empty_model_denies_everything_cross_agent() {
        let model = GrantModel::new();
        for verb in Verb::all() {
            assert!(!model.can(&agent("alice"), verb, "research.papers.doc1"));
            assert!(!model.can(&agent("alice"), verb, "system.agents.bob.files.x"));
        }
    }

    #[test]
    fn own_scope_needs_no_grant() {
        let model = GrantModel::new();
        assert!(model.can(&agent("alice"), Verb::Write, "system.agents.alice"));
        assert!(model.can(
            &agent("alice"),
            Verb::Delete,
            "system.agents.alice.files.notes"
        ));
        // Segment boundary: alice2 is not alice.
        assert!(!model.can(&agent("alice"), Verb::Read, "system.agents.alice2.files.x"));
    }

    // ---- scopes ----

    #[test]
    fn namespace_scope_is_segment_aware() {
        let ns = Scope::Namespace("research.papers".into());
        assert!(ns.covers("research.papers"));
        assert!(ns.covers("research.papers.doc1"));
        assert!(!ns.covers("research.papersX"));
        assert!(!ns.covers("research"));

        let name = Scope::Name("research.papers".into());
        assert!(name.covers("research.papers"));
        assert!(!name.covers("research.papers.doc1"));
    }

    #[test]
    fn scope_subset_relation() {
        let wide = Scope::Namespace("research".into());
        let narrow = Scope::Namespace("research.papers".into());
        let leaf = Scope::Name("research.papers.doc1".into());
        assert!(wide.covers_scope(&narrow));
        assert!(wide.covers_scope(&leaf));
        assert!(narrow.covers_scope(&leaf));
        assert!(!narrow.covers_scope(&wide));
        // A single name never covers a subtree.
        assert!(!leaf.covers_scope(&narrow));
        assert!(leaf.covers_scope(&Scope::Name("research.papers.doc1".into())));
    }

    // ---- operator grants ----

    #[test]
    fn operator_grant_allows_exactly_its_verbs_and_scope() {
        let mut model = GrantModel::new();
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            rw(),
            Scope::Namespace("research".into()),
        ));
        assert!(model.can(&agent("alice"), Verb::Read, "research.papers.doc1"));
        assert!(model.can(&agent("alice"), Verb::Write, "research"));
        assert!(!model.can(&agent("alice"), Verb::Delete, "research.papers.doc1")); // verb not granted
        assert!(!model.can(&agent("alice"), Verb::Read, "docs.readme")); // scope not covered
        assert!(!model.can(&agent("bob"), Verb::Read, "research.papers.doc1")); // wrong grantee
    }

    // ---- A3: revocation wins, transitively ----

    #[test]
    fn revocation_disables_a_direct_grant() {
        let mut model = GrantModel::new();
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            rw(),
            Scope::Namespace("research".into()),
        ));
        assert!(model.can(&agent("alice"), Verb::Read, "research.x"));
        model.apply(&GrantEvent::Revoked { id: 1 });
        assert!(!model.can(&agent("alice"), Verb::Read, "research.x"));
    }

    #[test]
    fn upstream_revocation_kills_the_delegated_subtree() {
        let mut model = GrantModel::new();
        // Operator -> alice (with Grant), alice -> bob, bob uses it.
        let mut with_grant = rw();
        with_grant.insert(Verb::Grant);
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            with_grant,
            Scope::Namespace("research".into()),
        ));
        model.apply(&grant(
            2,
            Grantor::Agent("alice".into()),
            "bob",
            BTreeSet::from([Verb::Read]),
            Scope::Namespace("research.papers".into()),
        ));
        assert!(model.can(&agent("bob"), Verb::Read, "research.papers.doc1"));

        model.apply(&GrantEvent::Revoked { id: 1 });
        assert!(
            !model.can(&agent("bob"), Verb::Read, "research.papers.doc1"),
            "delegation must die with its support"
        );
        assert!(!model.can(&agent("alice"), Verb::Read, "research.papers.doc1"));
    }

    #[test]
    fn revoking_an_unknown_id_is_inert_and_wins_over_late_grants() {
        let mut model = GrantModel::new();
        model.apply(&GrantEvent::Revoked { id: 7 });
        // The revocation of id 7 stands even though the grant arrives later.
        model.apply(&grant(
            7,
            Grantor::Operator,
            "alice",
            rw(),
            Scope::Namespace("research".into()),
        ));
        assert!(!model.can(&agent("alice"), Verb::Read, "research.x"));
    }

    // ---- A4: delegation is grant-gated and attenuated ----

    #[test]
    fn unauthorized_delegation_confers_nothing() {
        let mut model = GrantModel::new();
        // mallory holds nothing, yet "delegates" to bob.
        model.apply(&grant(
            1,
            Grantor::Agent("mallory".into()),
            "bob",
            rw(),
            Scope::Namespace("research".into()),
        ));
        assert!(!model.can(&agent("bob"), Verb::Read, "research.x"));
    }

    #[test]
    fn delegation_without_the_grant_verb_confers_nothing() {
        let mut model = GrantModel::new();
        // alice holds Read/Write (but not Grant) — she cannot delegate.
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            rw(),
            Scope::Namespace("research".into()),
        ));
        model.apply(&grant(
            2,
            Grantor::Agent("alice".into()),
            "bob",
            BTreeSet::from([Verb::Read]),
            Scope::Namespace("research".into()),
        ));
        assert!(!model.can(&agent("bob"), Verb::Read, "research.x"));
    }

    #[test]
    fn delegation_wider_than_the_delegator_confers_nothing() {
        let mut model = GrantModel::new();
        // alice holds Read + Grant over research.papers.
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            BTreeSet::from([Verb::Read, Verb::Grant]),
            Scope::Namespace("research.papers".into()),
        ));
        // Wider scope than alice holds -> inert (even inside her scope).
        model.apply(&grant(
            2,
            Grantor::Agent("alice".into()),
            "bob",
            BTreeSet::from([Verb::Read]),
            Scope::Namespace("research".into()),
        ));
        assert!(!model.can(&agent("bob"), Verb::Read, "research.other"));
        assert!(!model.can(&agent("bob"), Verb::Read, "research.papers.doc1"));

        // A verb alice does not hold (Write) -> inert.
        model.apply(&grant(
            3,
            Grantor::Agent("alice".into()),
            "carol",
            BTreeSet::from([Verb::Write]),
            Scope::Namespace("research.papers".into()),
        ));
        assert!(!model.can(&agent("carol"), Verb::Write, "research.papers.doc1"));
    }

    #[test]
    fn delegation_must_follow_its_support() {
        let mut model = GrantModel::new();
        // The delegation (id 1) precedes the supporting grant (id 2): inert —
        // authority must exist when delegated *and* still stand now.
        model.apply(&grant(
            1,
            Grantor::Agent("alice".into()),
            "bob",
            BTreeSet::from([Verb::Read]),
            Scope::Namespace("research".into()),
        ));
        let mut with_grant = rw();
        with_grant.insert(Verb::Grant);
        model.apply(&grant(
            2,
            Grantor::Operator,
            "alice",
            with_grant,
            Scope::Namespace("research".into()),
        ));
        assert!(!model.can(&agent("bob"), Verb::Read, "research.x"));
    }

    #[test]
    fn duplicate_grant_id_first_wins() {
        let mut model = GrantModel::new();
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            BTreeSet::from([Verb::Read]),
            Scope::Name("docs.readme".into()),
        ));
        model.apply(&grant(
            1,
            Grantor::Operator,
            "alice",
            Verb::all(),
            Scope::Namespace("docs".into()),
        ));
        assert!(model.can(&agent("alice"), Verb::Read, "docs.readme"));
        assert!(
            !model.can(&agent("alice"), Verb::Write, "docs.readme"),
            "the second (wider) event with a duplicate id must be ignored"
        );
    }

    // ---- property tests: the oracle over random event sequences ----

    /// A small closed universe keeps collisions (and therefore interesting
    /// interactions) frequent.
    const AGENTS: &[&str] = &["alice", "bob", "carol"];
    const NAMESPACES: &[&str] = &["research", "research.papers", "docs", "system.agents.alice"];
    const RESOURCES: &[&str] = &[
        "research",
        "research.papers",
        "research.papers.doc1",
        "research.papersX",
        "docs.readme",
        "system.agents.alice.files.x",
        "system.agents.bob.files.x",
    ];

    fn arb_verbs() -> impl Strategy<Value = BTreeSet<Verb>> {
        proptest::collection::btree_set(
            prop_oneof![
                Just(Verb::Read),
                Just(Verb::Write),
                Just(Verb::Delete),
                Just(Verb::List),
                Just(Verb::Grant)
            ],
            1..=5,
        )
    }

    fn arb_scope() -> impl Strategy<Value = Scope> {
        prop_oneof![
            proptest::sample::select(NAMESPACES).prop_map(|ns| Scope::Namespace(ns.into())),
            proptest::sample::select(RESOURCES).prop_map(|n| Scope::Name(n.into())),
        ]
    }

    fn arb_grantor() -> impl Strategy<Value = Grantor> {
        prop_oneof![
            Just(Grantor::Operator),
            proptest::sample::select(AGENTS).prop_map(|a| Grantor::Agent(a.into())),
        ]
    }

    /// A sequence of events with ids assigned in order and revocations aimed
    /// at plausibly-issued ids.
    fn arb_events(max: usize) -> impl Strategy<Value = Vec<GrantEvent>> {
        proptest::collection::vec(
            (
                arb_grantor(),
                proptest::sample::select(AGENTS),
                arb_verbs(),
                arb_scope(),
                any::<bool>(),
                0..max as u64,
            ),
            0..max,
        )
        .prop_map(|rows| {
            rows.into_iter()
                .enumerate()
                .map(|(i, (grantor, grantee, verbs, scope, revoke, target))| {
                    if revoke {
                        GrantEvent::Revoked { id: target }
                    } else {
                        GrantEvent::Granted {
                            id: i as u64,
                            grantor,
                            grantee: Principal::Agent(grantee.into()),
                            verbs,
                            scope,
                        }
                    }
                })
                .collect()
        })
    }

    /// Every decision over the sampled query grid.
    fn decisions(model: &GrantModel) -> Vec<bool> {
        let mut out = Vec::new();
        for a in AGENTS {
            for verb in Verb::all() {
                for r in RESOURCES {
                    out.push(model.can(&Principal::Agent((*a).into()), verb, r));
                }
            }
        }
        out
    }

    proptest! {
        /// A1 — a principal never named as grantee is denied everywhere
        /// outside its own scope, whatever the log says.
        #[test]
        fn fresh_principal_is_denied(events in arb_events(12)) {
            let model = GrantModel::replay(&events);
            let mallory = Principal::Agent("mallory".into());
            for verb in Verb::all() {
                for r in RESOURCES {
                    prop_assert!(!model.can(&mallory, verb, r), "mallory allowed {verb:?} on {r}");
                }
            }
            // Own scope still stands.
            prop_assert!(model.can(&mallory, Verb::Write, "system.agents.mallory.files.x"));
        }

        /// Granting is monotone: adding a grant never revokes anything.
        #[test]
        fn grants_never_shrink_the_allowed_set(
            events in arb_events(10),
            grantor in arb_grantor(),
            grantee in proptest::sample::select(AGENTS),
            verbs in arb_verbs(),
            scope in arb_scope(),
        ) {
            let before = GrantModel::replay(&events);
            let mut after = before.clone();
            after.apply(&GrantEvent::Granted {
                id: 1000, grantor, grantee: Principal::Agent(grantee.into()), verbs, scope,
            });
            for (b, a) in decisions(&before).into_iter().zip(decisions(&after)) {
                prop_assert!(!b || a, "a grant revoked a previously-allowed decision");
            }
        }

        /// A3 — revocation is monotone the other way: it never allows
        /// anything new.
        #[test]
        fn revocation_never_widens_the_allowed_set(events in arb_events(12), id in 0u64..12) {
            let before = GrantModel::replay(&events);
            let mut after = before.clone();
            after.apply(&GrantEvent::Revoked { id });
            for (b, a) in decisions(&before).into_iter().zip(decisions(&after)) {
                prop_assert!(!a || b, "a revocation allowed a previously-denied decision");
            }
        }

        /// A4 — a delegation by an agent holding no live Grant authority is
        /// inert: the allowed set is unchanged.
        #[test]
        fn unauthorized_delegation_is_inert(
            events in arb_events(10),
            grantee in proptest::sample::select(AGENTS),
            verbs in arb_verbs(),
            scope in arb_scope(),
        ) {
            let before = GrantModel::replay(&events);
            // "zed" never appears in the universe, so it can hold no authority.
            let mut after = before.clone();
            after.apply(&GrantEvent::Granted {
                id: 1000,
                grantor: Grantor::Agent("zed".into()),
                grantee: Principal::Agent(grantee.into()),
                verbs,
                scope,
            });
            prop_assert_eq!(decisions(&before), decisions(&after));
        }

        /// Replay determinism (the ground A5 stands on): the same log always
        /// produces the same decisions.
        #[test]
        fn replay_is_deterministic(events in arb_events(12)) {
            let a = GrantModel::replay(&events);
            let b = GrantModel::replay(&events);
            prop_assert_eq!(decisions(&a), decisions(&b));
        }
    }
}
