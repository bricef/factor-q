//! The durable grant-event log and its fan-out bus (M2 slice 2).
//!
//! Grants are event-sourced (ADR-0023 F4). This module supplies the log and
//! its transport, split so that **bus failure can never affect store
//! availability** (claim A6 in the M2 plan):
//!
//! - [`SqliteGrantLog`] — the **authoritative**, locally-durable event log
//!   (SQLite database #2 per ADR-0024, alongside the storage index). Appends
//!   assign the [`GrantId`]s; replay feeds the projection and the reference
//!   model. Nothing about appending or replaying touches the network.
//! - [`GrantBus`] — the fan-out seam: a publish-only trait carrying
//!   [`WireGrantEvent`] envelopes to external consumers (audit, other
//!   services). [`InMemoryGrantBus`] backs tests (with an outage toggle);
//!   `NatsGrantBus` (feature `bus`) publishes to NATS JetStream.
//! - [`drain`] — the outbox pump: publishes not-yet-published events in log
//!   order, marking each on acknowledgement. A bus outage leaves events
//!   queued (`published = 0`); a later drain catches up. Appends never wait.
//!
//! This refines ADR-0023's dataflow deliberately: the projection consumes the
//! **local** log, not the bus, so rebuild (claim A5) never depends on a
//! broker. The bus is a feed, not the source of truth.
//!
//! Wire naming (settled here, per the plan): schema ids
//! `factor-q/granted@1` (operator grants), `factor-q/delegated@1` (agent
//! grants), `factor-q/revoked@1`; subjects `fq.store.grant.granted` /
//! `.delegated` / `.revoked`.

use std::collections::BTreeSet;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::grants::{GrantEvent, GrantId, Grantor, Principal, Scope, Verb};
use crate::index::now_millis;
use crate::{Result, StoreError};

/// Schema migrations for the grants database, applied in order (the same
/// `PRAGMA user_version` pattern as the storage index — see
/// [`crate::index`]).
const MIGRATIONS: &[&str] = &[
    // v1 — the event log. `seq` is the log position AND the GrantId of a
    // `granted` row (AUTOINCREMENT: monotone, never reused). `published`
    // marks fan-out progress: 0 = pending (the outbox), 1 = acknowledged by
    // the bus.
    "CREATE TABLE grant_events (
        seq           INTEGER PRIMARY KEY AUTOINCREMENT,
        kind          TEXT    NOT NULL CHECK (kind IN ('granted', 'revoked')),
        target_id     INTEGER,
        grantor_agent TEXT,
        grantee_agent TEXT,
        verbs         TEXT,
        scope_kind    TEXT CHECK (scope_kind IN ('name', 'namespace')),
        scope_value   TEXT,
        occurred_at   INTEGER NOT NULL,
        published     INTEGER NOT NULL DEFAULT 0
    );",
];

/// The authoritative, locally-durable grant-event log (SQLite database #2).
/// Single-connection for the same reason as the storage index: every append
/// linearizes, and WAL's `BUSY_SNAPSHOT` cannot arise.
pub struct SqliteGrantLog {
    pool: SqlitePool,
}

impl SqliteGrantLog {
    /// Open (or create) the grants database at `path` and run migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Append a grant, assigning its [`GrantId`] (the log sequence). Durable
    /// on return; queued for fan-out.
    pub async fn append_granted(
        &self,
        grantor: &Grantor,
        grantee: &Principal,
        verbs: &BTreeSet<Verb>,
        scope: &Scope,
    ) -> Result<GrantId> {
        let grantor_agent = match grantor {
            Grantor::Operator => None,
            Grantor::Agent(id) => Some(id.clone()),
        };
        let Principal::Agent(grantee_agent) = grantee;
        let verbs_json =
            serde_json::to_string(verbs).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let (scope_kind, scope_value) = match scope {
            Scope::Name(name) => ("name", name.clone()),
            Scope::Namespace(ns) => ("namespace", ns.clone()),
        };
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO grant_events
                 (kind, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, occurred_at)
             VALUES ('granted', ?, ?, ?, ?, ?, ?) RETURNING seq",
        )
        .bind(grantor_agent)
        .bind(grantee_agent)
        .bind(verbs_json)
        .bind(scope_kind)
        .bind(scope_value)
        .bind(now_millis())
        .fetch_one(&self.pool)
        .await?;
        Ok(seq as GrantId)
    }

    /// Append a revocation of the grant `target`. Durable on return; queued
    /// for fan-out.
    pub async fn append_revoked(&self, target: GrantId) -> Result<()> {
        sqlx::query(
            "INSERT INTO grant_events (kind, target_id, occurred_at) VALUES ('revoked', ?, ?)",
        )
        .bind(target as i64)
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every event, in log order — the input to a projection rebuild (A5) and
    /// to [`crate::GrantModel::replay`]. Never touches the bus.
    pub async fn replay(&self) -> Result<Vec<GrantEvent>> {
        let rows = self.rows("SELECT seq, kind, target_id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, occurred_at FROM grant_events ORDER BY seq").await?;
        rows.into_iter().map(|row| row.into_event()).collect()
    }

    /// The outbox: not-yet-published events as wire envelopes, in log order.
    pub async fn pending(&self) -> Result<Vec<WireGrantEvent>> {
        let rows = self.rows("SELECT seq, kind, target_id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, occurred_at FROM grant_events WHERE published = 0 ORDER BY seq").await?;
        rows.into_iter().map(|row| row.into_envelope()).collect()
    }

    /// Record that the bus acknowledged the event at `seq`.
    pub async fn mark_published(&self, seq: u64) -> Result<()> {
        sqlx::query("UPDATE grant_events SET published = 1 WHERE seq = ?")
            .bind(seq as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn rows(&self, sql: &str) -> Result<Vec<EventRow>> {
        let rows: Vec<EventTuple> = sqlx::query_as(sql).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    seq,
                    kind,
                    target_id,
                    grantor_agent,
                    grantee_agent,
                    verbs,
                    scope_kind,
                    scope_value,
                    occurred_at,
                )| EventRow {
                    seq,
                    kind,
                    target_id,
                    grantor_agent,
                    grantee_agent,
                    verbs,
                    scope_kind,
                    scope_value,
                    occurred_at,
                },
            )
            .collect())
    }
}

/// A raw `grant_events` row as sqlx fetches it: `(seq, kind, target_id,
/// grantor_agent, grantee_agent, verbs, scope_kind, scope_value, occurred_at)`.
type EventTuple = (
    i64,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

/// One `grant_events` row, decoded lazily into domain or wire form.
struct EventRow {
    seq: i64,
    kind: String,
    target_id: Option<i64>,
    grantor_agent: Option<String>,
    grantee_agent: Option<String>,
    verbs: Option<String>,
    scope_kind: Option<String>,
    scope_value: Option<String>,
    occurred_at: i64,
}

impl EventRow {
    fn into_event(self) -> Result<GrantEvent> {
        let corrupt = |what: &str, seq: i64| {
            StoreError::Corrupt(format!("grant_events row {seq}: missing/invalid {what}"))
        };
        match self.kind.as_str() {
            "granted" => {
                let grantor = match self.grantor_agent {
                    Some(id) => Grantor::Agent(id),
                    None => Grantor::Operator,
                };
                let grantee = Principal::Agent(
                    self.grantee_agent
                        .ok_or_else(|| corrupt("grantee", self.seq))?,
                );
                let verbs: BTreeSet<Verb> = serde_json::from_str(
                    self.verbs
                        .as_deref()
                        .ok_or_else(|| corrupt("verbs", self.seq))?,
                )
                .map_err(|_| corrupt("verbs", self.seq))?;
                let value = self.scope_value.ok_or_else(|| corrupt("scope", self.seq))?;
                let scope = match self.scope_kind.as_deref() {
                    Some("name") => Scope::Name(value),
                    Some("namespace") => Scope::Namespace(value),
                    _ => return Err(corrupt("scope kind", self.seq)),
                };
                Ok(GrantEvent::Granted {
                    id: self.seq as GrantId,
                    grantor,
                    grantee,
                    verbs,
                    scope,
                })
            }
            "revoked" => Ok(GrantEvent::Revoked {
                id: self.target_id.ok_or_else(|| corrupt("target", self.seq))? as GrantId,
            }),
            _ => Err(corrupt("kind", self.seq)),
        }
    }

    fn into_envelope(self) -> Result<WireGrantEvent> {
        let seq = self.seq as u64;
        let occurred_at_ms = self.occurred_at;
        let event = self.into_event()?;
        Ok(WireGrantEvent {
            schema_id: schema_id(&event).to_string(),
            seq,
            occurred_at_ms,
            event,
        })
    }
}

/// The stable payload-schema id for an event (`factor-q/<kind>@1`): operator
/// grants are `granted`, agent grants (delegations) are `delegated`.
fn schema_id(event: &GrantEvent) -> &'static str {
    match event {
        GrantEvent::Granted {
            grantor: Grantor::Operator,
            ..
        } => "factor-q/granted@1",
        GrantEvent::Granted { .. } => "factor-q/delegated@1",
        GrantEvent::Revoked { .. } => "factor-q/revoked@1",
    }
}

/// A grant event as published on the bus: the store's compact envelope
/// (schema id, log sequence, timestamp) around the domain event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireGrantEvent {
    /// Payload schema id, e.g. `factor-q/granted@1`.
    pub schema_id: String,
    /// The log sequence — a `granted` event's seq is its [`GrantId`].
    pub seq: u64,
    /// When the event was appended (Unix milliseconds).
    pub occurred_at_ms: i64,
    /// The domain event.
    pub event: GrantEvent,
}

impl WireGrantEvent {
    /// The NATS subject this event publishes to (`fq.store.grant.<kind>`).
    pub fn subject(&self) -> String {
        let kind = match &self.event {
            GrantEvent::Granted {
                grantor: Grantor::Operator,
                ..
            } => "granted",
            GrantEvent::Granted { .. } => "delegated",
            GrantEvent::Revoked { .. } => "revoked",
        };
        format!("fq.store.grant.{kind}")
    }
}

/// The fan-out seam: publish one envelope to external consumers. Publish-only
/// by design — nothing in the store ever *reads* the bus (A6); rebuild reads
/// the local log (A5).
#[async_trait]
pub trait GrantBus: Send + Sync {
    /// Durably publish `event`; return once the transport acknowledges it.
    async fn publish(&self, event: &WireGrantEvent) -> Result<()>;
}

/// Publish everything pending, in log order, marking each event on
/// acknowledgement. Returns how many were published. On a bus error the
/// already-acknowledged prefix stays marked and the rest stays queued — a
/// later drain resumes exactly where this one stopped.
pub async fn drain(log: &SqliteGrantLog, bus: &dyn GrantBus) -> Result<usize> {
    let mut published = 0;
    for event in log.pending().await? {
        bus.publish(&event).await?;
        log.mark_published(event.seq).await?;
        published += 1;
    }
    Ok(published)
}

/// An in-memory [`GrantBus`] for tests: records published envelopes, and can
/// simulate an outage ([`set_down`](Self::set_down)) to exercise A6.
#[derive(Default)]
pub struct InMemoryGrantBus {
    published: std::sync::Mutex<Vec<WireGrantEvent>>,
    down: std::sync::atomic::AtomicBool,
}

impl InMemoryGrantBus {
    /// A healthy, empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate (or heal) an outage: while down, every publish fails.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, std::sync::atomic::Ordering::SeqCst);
    }

    /// Everything published so far, in publish order.
    pub fn published(&self) -> Vec<WireGrantEvent> {
        self.published.lock().expect("bus mutex").clone()
    }
}

#[async_trait]
impl GrantBus for InMemoryGrantBus {
    async fn publish(&self, event: &WireGrantEvent) -> Result<()> {
        if self.down.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Bus("simulated outage".into()));
        }
        self.published
            .lock()
            .expect("bus mutex")
            .push(event.clone());
        Ok(())
    }
}

/// The NATS JetStream [`GrantBus`] (feature `bus`): publishes each envelope as
/// JSON to `<subject_prefix>.<kind>` and awaits the JetStream ack.
#[cfg(feature = "bus")]
pub mod nats {
    use super::*;

    /// A publish-only JetStream client for the grant feed.
    pub struct NatsGrantBus {
        js: async_nats::jetstream::Context,
        subject_prefix: String,
    }

    impl NatsGrantBus {
        /// Connect to NATS at `url` and ensure the stream `stream` exists,
        /// capturing `"<subject_prefix>.>"`. Production defaults: stream
        /// `FQ_GRANTS`, prefix `fq.store.grant`.
        pub async fn connect(url: &str, stream: &str, subject_prefix: &str) -> Result<Self> {
            let client = async_nats::connect(url)
                .await
                .map_err(|e| StoreError::Bus(format!("connect {url}: {e}")))?;
            let js = async_nats::jetstream::new(client);
            js.get_or_create_stream(async_nats::jetstream::stream::Config {
                name: stream.to_string(),
                subjects: vec![format!("{subject_prefix}.>")],
                ..Default::default()
            })
            .await
            .map_err(|e| StoreError::Bus(format!("stream {stream}: {e}")))?;
            Ok(Self {
                js,
                subject_prefix: subject_prefix.to_string(),
            })
        }

        fn subject_for(&self, event: &WireGrantEvent) -> String {
            // The envelope computes the canonical `fq.store.grant.<kind>`;
            // re-prefix so tests can publish to an isolated subject space.
            let kind = event.subject();
            let kind = kind.rsplit('.').next().unwrap_or("event");
            format!("{}.{kind}", self.subject_prefix)
        }
    }

    #[async_trait]
    impl GrantBus for NatsGrantBus {
        async fn publish(&self, event: &WireGrantEvent) -> Result<()> {
            let payload = serde_json::to_vec(event).map_err(|e| StoreError::Bus(e.to_string()))?;
            self.js
                .publish(self.subject_for(event), payload.into())
                .await
                .map_err(|e| StoreError::Bus(format!("publish: {e}")))?
                .await
                .map_err(|e| StoreError::Bus(format!("ack: {e}")))?;
            Ok(())
        }
    }
}

/// Apply pending migrations (the storage-index pattern: each migration and
/// its version bump commit atomically).
async fn migrate(pool: &SqlitePool) -> Result<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let target = i as i64 + 1;
        if version < target {
            let mut tx = pool.begin().await?;
            sqlx::query(migration).execute(&mut *tx).await?;
            sqlx::query(&format!("PRAGMA user_version = {target}"))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::GrantModel;
    use proptest::prelude::*;

    async fn log() -> (tempfile::TempDir, SqliteGrantLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = SqliteGrantLog::open(dir.path().join("grants.db"))
            .await
            .unwrap();
        (dir, log)
    }

    fn rw() -> BTreeSet<Verb> {
        BTreeSet::from([Verb::Read, Verb::Write])
    }

    #[tokio::test]
    async fn appends_assign_sequential_ids_and_replay_in_order() {
        let (_d, log) = log().await;
        let a = log
            .append_granted(
                &Grantor::Operator,
                &Principal::Agent("alice".into()),
                &rw(),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        let b = log
            .append_granted(
                &Grantor::Agent("alice".into()),
                &Principal::Agent("bob".into()),
                &BTreeSet::from([Verb::Read]),
                &Scope::Name("research.papers.doc1".into()),
            )
            .await
            .unwrap();
        log.append_revoked(a).await.unwrap();
        assert!(a < b, "ids are the log order");

        let events = log.replay().await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0],
            GrantEvent::Granted {
                id: a,
                grantor: Grantor::Operator,
                grantee: Principal::Agent("alice".into()),
                verbs: rw(),
                scope: Scope::Namespace("research".into()),
            }
        );
        assert_eq!(
            events[1],
            GrantEvent::Granted {
                id: b,
                grantor: Grantor::Agent("alice".into()),
                grantee: Principal::Agent("bob".into()),
                verbs: BTreeSet::from([Verb::Read]),
                scope: Scope::Name("research.papers.doc1".into()),
            }
        );
        assert_eq!(events[2], GrantEvent::Revoked { id: a });
    }

    #[tokio::test]
    async fn the_log_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.db");
        let log = SqliteGrantLog::open(&path).await.unwrap();
        log.append_granted(
            &Grantor::Operator,
            &Principal::Agent("alice".into()),
            &rw(),
            &Scope::Namespace("research".into()),
        )
        .await
        .unwrap();
        drop(log);

        // Reopen on the same file: events and outbox state survive.
        let log = SqliteGrantLog::open(&path).await.unwrap();
        assert_eq!(log.replay().await.unwrap().len(), 1);
        assert_eq!(log.pending().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn drain_publishes_in_order_and_is_idempotent() {
        let (_d, log) = log().await;
        let bus = InMemoryGrantBus::new();
        let id = log
            .append_granted(
                &Grantor::Operator,
                &Principal::Agent("alice".into()),
                &rw(),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        log.append_revoked(id).await.unwrap();

        assert_eq!(drain(&log, &bus).await.unwrap(), 2);
        let published = bus.published();
        assert_eq!(published.len(), 2);
        assert!(published[0].seq < published[1].seq);
        assert_eq!(published[0].schema_id, "factor-q/granted@1");
        assert_eq!(published[1].schema_id, "factor-q/revoked@1");
        assert_eq!(published[0].subject(), "fq.store.grant.granted");

        // Nothing left: a second drain publishes nothing new.
        assert_eq!(drain(&log, &bus).await.unwrap(), 0);
        assert_eq!(bus.published().len(), 2);
    }

    #[tokio::test]
    async fn a_bus_outage_never_blocks_appends_and_drain_catches_up() {
        let (_d, log) = log().await;
        let bus = InMemoryGrantBus::new();

        // Publish one healthy event, then the bus goes down.
        log.append_granted(
            &Grantor::Operator,
            &Principal::Agent("alice".into()),
            &rw(),
            &Scope::Namespace("research".into()),
        )
        .await
        .unwrap();
        assert_eq!(drain(&log, &bus).await.unwrap(), 1);
        bus.set_down(true);

        // A6: appends keep succeeding while the bus is down; drains fail but
        // change nothing about the log's contents.
        let id = log
            .append_granted(
                &Grantor::Agent("alice".into()),
                &Principal::Agent("bob".into()),
                &BTreeSet::from([Verb::Read]),
                &Scope::Namespace("research.papers".into()),
            )
            .await
            .unwrap();
        log.append_revoked(id).await.unwrap();
        assert!(matches!(drain(&log, &bus).await, Err(StoreError::Bus(_))));
        assert_eq!(log.pending().await.unwrap().len(), 2);
        assert_eq!(
            log.replay().await.unwrap().len(),
            3,
            "the log is unaffected"
        );

        // The bus heals: one drain catches up, in order, exactly once each.
        bus.set_down(false);
        assert_eq!(drain(&log, &bus).await.unwrap(), 2);
        assert_eq!(log.pending().await.unwrap().len(), 0);
        let seqs: Vec<u64> = bus.published().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, {
            let mut sorted = seqs.clone();
            sorted.sort_unstable();
            sorted
        });
        assert_eq!(bus.published().len(), 3);
    }

    #[tokio::test]
    async fn delegated_events_carry_the_delegated_schema_id() {
        let (_d, log) = log().await;
        log.append_granted(
            &Grantor::Agent("alice".into()),
            &Principal::Agent("bob".into()),
            &BTreeSet::from([Verb::Read]),
            &Scope::Namespace("research".into()),
        )
        .await
        .unwrap();
        let pending = log.pending().await.unwrap();
        assert_eq!(pending[0].schema_id, "factor-q/delegated@1");
        assert_eq!(pending[0].subject(), "fq.store.grant.delegated");
    }

    #[tokio::test]
    async fn wire_envelopes_round_trip_through_json() {
        let (_d, log) = log().await;
        log.append_granted(
            &Grantor::Operator,
            &Principal::Agent("alice".into()),
            &Verb::all(),
            &Scope::Name("docs.readme".into()),
        )
        .await
        .unwrap();
        let envelope = log.pending().await.unwrap().remove(0);
        let json = serde_json::to_string(&envelope).unwrap();
        let back: WireGrantEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
    }

    /// The A5 groundwork: any appended sequence replays to exactly the domain
    /// events appended (ids included), and feeding that replay to the
    /// reference model is well-defined. Sequences mix operator grants, agent
    /// grants, and revocations of plausible ids.
    #[test]
    fn replay_round_trips_arbitrary_sequences() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let strategy = proptest::collection::vec(
            (
                any::<bool>(),
                proptest::sample::select(&["alice", "bob", "carol"][..]),
                proptest::sample::select(&["alice", "bob", "carol"][..]),
                proptest::collection::btree_set(
                    prop_oneof![
                        Just(Verb::Read),
                        Just(Verb::Write),
                        Just(Verb::Delete),
                        Just(Verb::List),
                        Just(Verb::Grant)
                    ],
                    1..=5,
                ),
                any::<bool>(),
                proptest::sample::select(&["research", "research.papers", "docs.readme"][..]),
                any::<bool>(),
                1u64..8,
            ),
            0..8,
        );
        let mut runner =
            proptest::test_runner::TestRunner::new(proptest::test_runner::Config::with_cases(32));
        runner
            .run(&strategy, |rows| {
                rt.block_on(async {
                    let (_d, log) = log().await;
                    let mut expected = Vec::new();
                    for (
                        op_grantor,
                        grantor,
                        grantee,
                        verbs,
                        name_scope,
                        scope_val,
                        revoke,
                        target,
                    ) in rows
                    {
                        if revoke {
                            log.append_revoked(target).await.unwrap();
                            expected.push(GrantEvent::Revoked { id: target });
                        } else {
                            let grantor = if op_grantor {
                                Grantor::Operator
                            } else {
                                Grantor::Agent(grantor.to_string())
                            };
                            let scope = if name_scope {
                                Scope::Name(scope_val.to_string())
                            } else {
                                Scope::Namespace(scope_val.to_string())
                            };
                            let grantee = Principal::Agent(grantee.to_string());
                            let id = log
                                .append_granted(&grantor, &grantee, &verbs, &scope)
                                .await
                                .unwrap();
                            expected.push(GrantEvent::Granted {
                                id,
                                grantor,
                                grantee,
                                verbs,
                                scope,
                            });
                        }
                    }
                    let replayed = log.replay().await.unwrap();
                    assert_eq!(replayed, expected);
                    // Replay feeds the reference model without complaint.
                    let _ = GrantModel::replay(&replayed);
                });
                Ok(())
            })
            .unwrap();
    }
}
