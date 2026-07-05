//! Deterministic-simulation harness for the access-control stack (M2 slice 7).
//!
//! A seeded workload drives the real grants machinery — `SqliteGrantLog`
//! (log plus projection), the outbox `drain`, an outage-injectable bus, and live
//! capability tokens — and checks the M2 claims after every step:
//!
//! - **A5 (projection ≡ replay):** the projection's decisions equal the
//!   reference [`GrantModel`] replayed from the log, over a query grid — after
//!   every event, across crash-reopens, and after mid-stream full rebuilds.
//! - **A6 (availability):** grant appends never fail while the bus is down;
//!   the outbox conserves events (published ∪ pending = everything appended,
//!   in order) and a healed bus drains exactly what was queued.
//! - **Belt-and-braces under revocation races:** for tokens minted at
//!   arbitrary earlier steps, `token.permits ∧ projection.can` equals the
//!   model's decision at every step — a revocation is effective on the very
//!   next check no matter how fresh the token is.
//! - **Crash-replay:** dropping and reopening the grants DB (the WAL crash
//!   model, as in the storage DST) loses nothing: appends are durable when
//!   they return, and `open`'s catch-up restores the projection.
//!
//! The RNG is a self-contained splitmix64: any failure reproduces exactly
//! from its printed `seed` and `step`. Crank it for a soak with
//! `FQ_GRANTS_SIM_SEEDS=64 FQ_GRANTS_SIM_STEPS=80 cargo test --test grants_sim`.
//!
//! (The drain is publish-then-mark, so a crash *between* those two would
//! re-publish on the next drain — at-least-once fan-out by design. The DST
//! never crashes mid-drain, so it asserts exact conservation; consumers must
//! de-duplicate by `seq` regardless.)

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use fq_store::{
    GrantEvent, GrantModel, Grantor, InMemoryGrantBus, Principal, Scope, SqliteGrantLog,
    StoreError, TokenMinter, TokenVerifier, Verb, VerifiedToken, drain, generate_keypair,
};

fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn below(state: &mut u64, n: u64) -> u64 {
    next_u64(state) % n
}

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

fn pick<'a>(rng: &mut u64, pool: &[&'a str]) -> &'a str {
    pool[below(rng, pool.len() as u64) as usize]
}

fn random_verbs(rng: &mut u64) -> BTreeSet<Verb> {
    let all = [
        Verb::Read,
        Verb::Write,
        Verb::Delete,
        Verb::List,
        Verb::Grant,
    ];
    let mut verbs = BTreeSet::new();
    for verb in all {
        if below(rng, 2) == 0 {
            verbs.insert(verb);
        }
    }
    if verbs.is_empty() {
        verbs.insert(Verb::Read);
    }
    verbs
}

fn random_scope(rng: &mut u64) -> Scope {
    if below(rng, 2) == 0 {
        Scope::Namespace(pick(rng, NAMESPACES).to_string())
    } else {
        Scope::Name(pick(rng, RESOURCES).to_string())
    }
}

/// The model's decisions over the whole query grid.
fn model_grid(model: &GrantModel) -> Vec<bool> {
    let mut out = Vec::new();
    for agent in AGENTS {
        for verb in Verb::all() {
            for resource in RESOURCES {
                out.push(model.can(&Principal::Agent((*agent).into()), verb, resource));
            }
        }
    }
    out
}

/// The projection's decisions over the same grid.
async fn store_grid(log: &SqliteGrantLog) -> Vec<bool> {
    let mut out = Vec::new();
    for agent in AGENTS {
        for verb in Verb::all() {
            for resource in RESOURCES {
                out.push(
                    log.can(&Principal::Agent((*agent).into()), verb, resource)
                        .await
                        .unwrap(),
                );
            }
        }
    }
    out
}

/// A token held across steps: who it identifies plus the verified handle.
struct HeldToken {
    principal: Principal,
    verified: VerifiedToken,
}

async fn open_log(path: &Path) -> SqliteGrantLog {
    SqliteGrantLog::open(path).await.unwrap()
}

async fn run_seed(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("grants.db");
    let mut log = open_log(&db).await;

    // The bus is an external broker: it survives our "crashes".
    let bus = InMemoryGrantBus::new();
    let mut bus_down = false;

    // One keypair per world; generous TTL so expiry never interferes (TTL
    // semantics are unit-tested at the token layer).
    let (private, public) = generate_keypair();
    let minter = TokenMinter::from_private_key_hex(&private, Duration::from_secs(3600)).unwrap();
    let verifier = TokenVerifier::from_public_key_hex(&public).unwrap();
    let mut tokens: Vec<HeldToken> = Vec::new();

    // The lockstep differential model + the exact event mirror.
    let mut model = GrantModel::new();
    let mut mirror: Vec<GrantEvent> = Vec::new();
    let mut max_id: u64 = 0;

    let mut rng = seed ^ 0xA02B_DBF7_BB3C_0A7A;
    for step in 0..steps {
        match below(&mut rng, 12) {
            // Operator grant.
            0..=2 => {
                let grantee = Principal::Agent(pick(&mut rng, AGENTS).to_string());
                let verbs = random_verbs(&mut rng);
                let scope = random_scope(&mut rng);
                let id = log
                    .append_granted(&Grantor::Operator, &grantee, &verbs, &scope)
                    .await
                    .unwrap();
                let event = GrantEvent::Granted {
                    id,
                    grantor: Grantor::Operator,
                    grantee,
                    verbs,
                    scope,
                };
                model.apply(&event);
                mirror.push(event);
                max_id = max_id.max(id);
            }
            // Agent "delegation" — deliberately unchecked (the log tolerates
            // garbage; live-vs-inert is the projection's job to get right).
            3..=4 => {
                let grantor = Grantor::Agent(pick(&mut rng, AGENTS).to_string());
                let grantee = Principal::Agent(pick(&mut rng, AGENTS).to_string());
                let verbs = random_verbs(&mut rng);
                let scope = random_scope(&mut rng);
                let id = log
                    .append_granted(&grantor, &grantee, &verbs, &scope)
                    .await
                    .unwrap();
                let event = GrantEvent::Granted {
                    id,
                    grantor,
                    grantee,
                    verbs,
                    scope,
                };
                model.apply(&event);
                mirror.push(event);
                max_id = max_id.max(id);
            }
            // Revocation of a plausible (or bogus) id.
            5..=6 => {
                let target = below(&mut rng, max_id + 2);
                log.append_revoked(target).await.unwrap();
                let event = GrantEvent::Revoked { id: target };
                model.apply(&event);
                mirror.push(event);
            }
            // Crash: drop the log handle and reopen the same file. Everything
            // appended so far was durable on return, so the model carries
            // over unchanged.
            7 => {
                log = open_log(&db).await;
            }
            // Bus outage / heal.
            8 => {
                bus_down = !bus_down;
                bus.set_down(bus_down);
            }
            // Drain the outbox. Down with work queued ⇒ a Bus error and
            // nothing lost; down with an empty outbox ⇒ a trivial Ok(0) (no
            // publish is attempted); up ⇒ pending publishes.
            9 => {
                let had_pending = !log.pending().await.unwrap().is_empty();
                let result = drain(&log, &bus).await;
                if bus_down && had_pending {
                    assert!(
                        matches!(result, Err(StoreError::Bus(_))),
                        "seed={seed} step={step}: drain must fail while the bus is down"
                    );
                } else {
                    result.unwrap();
                }
            }
            // Mint a token for a random agent (kept and re-checked every step).
            10 => {
                let principal = Principal::Agent(pick(&mut rng, AGENTS).to_string());
                let token = minter.mint_for(&log, &principal).await.unwrap();
                let verified = verifier.verify(&token).unwrap();
                tokens.push(HeldToken {
                    principal,
                    verified,
                });
                if tokens.len() > 4 {
                    tokens.remove(0);
                }
            }
            // Full projection rebuild, mid-stream.
            _ => {
                log.rebuild_projection().await.unwrap();
            }
        }

        // A5: the projection equals the lockstep model — and the log's own
        // replay equals the mirror (nothing bent on the way to disk).
        let expected = model_grid(&model);
        assert_eq!(
            store_grid(&log).await,
            expected,
            "seed={seed} step={step}: projection diverged from the model"
        );
        let replayed = log.replay().await.unwrap();
        assert_eq!(
            replayed, mirror,
            "seed={seed} step={step}: the log's replay diverged from the appended events"
        );

        // A6 conservation: published ∪ pending = every event, in order, with
        // no duplicates — regardless of outages and crashes so far.
        let published: Vec<u64> = bus.published().iter().map(|e| e.seq).collect();
        let pending: Vec<u64> = log.pending().await.unwrap().iter().map(|e| e.seq).collect();
        let mut all: Vec<u64> = published.iter().chain(pending.iter()).copied().collect();
        all.sort_unstable();
        assert_eq!(
            all,
            (1..=mirror.len() as u64).collect::<Vec<_>>(),
            "seed={seed} step={step}: outbox conservation broken"
        );
        assert!(
            published.windows(2).all(|w| w[0] < w[1]),
            "seed={seed} step={step}: bus feed out of order"
        );

        // Belt-and-braces under revocation races: for every held token —
        // however stale — the gate's composition equals the live model.
        let now = SystemTime::now();
        for held in &tokens {
            for _ in 0..3 {
                let verb = *Verb::all().iter().nth(below(&mut rng, 5) as usize).unwrap();
                let resource = pick(&mut rng, RESOURCES);
                let composed = held.verified.permits(verb, resource, now)
                    && log.can(&held.principal, verb, resource).await.unwrap();
                assert_eq!(
                    composed,
                    model.can(&held.principal, verb, resource),
                    "seed={seed} step={step}: belt-and-braces diverged for {:?} {verb:?} {resource}",
                    held.principal
                );
            }
        }
    }

    // End of seed: heal the bus, drain fully, and the feed must hold exactly
    // every event, in order; a final rebuild must change no decision.
    bus.set_down(false);
    drain(&log, &bus).await.unwrap();
    assert!(
        log.pending().await.unwrap().is_empty(),
        "seed={seed}: outbox not empty"
    );
    let published: Vec<u64> = bus.published().iter().map(|e| e.seq).collect();
    assert_eq!(
        published,
        (1..=mirror.len() as u64).collect::<Vec<_>>(),
        "seed={seed}: final feed incomplete or out of order"
    );
    let before = store_grid(&log).await;
    log.rebuild_projection().await.unwrap();
    assert_eq!(
        store_grid(&log).await,
        before,
        "seed={seed}: rebuild changed decisions"
    );
    assert_eq!(
        before,
        model_grid(&model),
        "seed={seed}: final state diverged"
    );
}

/// Quick by default for the CI gate; crank it for a soak run with
/// `FQ_GRANTS_SIM_SEEDS=64 FQ_GRANTS_SIM_STEPS=80 cargo test --test grants_sim`.
#[tokio::test]
async fn dst_grants_hold_claims_under_faults() {
    fn env_or(key: &str, default: u64) -> u64 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    let seeds = env_or("FQ_GRANTS_SIM_SEEDS", 12);
    let steps = env_or("FQ_GRANTS_SIM_STEPS", 40) as usize;
    for seed in 0..seeds {
        run_seed(seed, steps).await;
    }
}
