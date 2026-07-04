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
//! - The **projection** (M2 slice 3) — the queryable current-permission state
//!   ([`can`](SqliteGrantLog::can), [`live_grants_for`](SqliteGrantLog::live_grants_for)),
//!   stored beside the log and updated **in the same transaction** as every
//!   append, so projection ≡ replay at every commit point (A5). Liveness (the
//!   delegation-chain rule from [`crate::GrantModel`]) is cached per grant and
//!   recomputed on each event; [`rebuild_projection`](SqliteGrantLog::rebuild_projection)
//!   re-derives everything from the log, and `open` catches up a stale
//!   projection via the applied-seq cursor.
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
    // v2 — the grant projection (M2 slice 3): the queryable current-permission
    // state, derived from the log and updated in the same transaction as every
    // append, so projection ≡ replay at every commit point (A5).
    // `projected_grants.live` caches chain liveness; `projected_revocations`
    // mirrors the model's revoked set (an id can be revoked before any grant
    // carries it); `projection_cursor` records the last log seq applied, so
    // `open` can idempotently catch up a projection created by an older binary
    // or wiped for rebuild.
    "CREATE TABLE projected_grants (
        id            INTEGER PRIMARY KEY,
        grantor_agent TEXT,
        grantee_agent TEXT    NOT NULL,
        verbs         TEXT    NOT NULL,
        scope_kind    TEXT    NOT NULL CHECK (scope_kind IN ('name', 'namespace')),
        scope_value   TEXT    NOT NULL,
        live          INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_projected_grants_grantee ON projected_grants(grantee_agent);
    CREATE TABLE projected_revocations (id INTEGER PRIMARY KEY);
    CREATE TABLE projection_cursor (
        id          INTEGER PRIMARY KEY CHECK (id = 1),
        applied_seq INTEGER NOT NULL
    );
    INSERT INTO projection_cursor (id, applied_seq) VALUES (1, 0);",
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
        let log = Self { pool };
        // Catch the projection up with the log (a no-op when they are already
        // level): idempotent recovery for a database written before the
        // projection existed, or wiped for rebuild.
        log.catch_up().await?;
        Ok(log)
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
        // Log append and projection update commit atomically: at every commit
        // point the projection equals a replay of the log (A5).
        let mut tx = self.pool.begin().await?;
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO grant_events
                 (kind, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, occurred_at)
             VALUES ('granted', ?, ?, ?, ?, ?, ?) RETURNING seq",
        )
        .bind(&grantor_agent)
        .bind(grantee_agent)
        .bind(&verbs_json)
        .bind(scope_kind)
        .bind(&scope_value)
        .bind(now_millis())
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO projected_grants
                 (id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(seq)
        .bind(&grantor_agent)
        .bind(grantee_agent)
        .bind(&verbs_json)
        .bind(scope_kind)
        .bind(&scope_value)
        .execute(&mut *tx)
        .await?;
        recompute_liveness(&mut tx).await?;
        set_cursor(&mut tx, seq).await?;
        tx.commit().await?;
        Ok(seq as GrantId)
    }

    /// Append a revocation of the grant `target`. Durable on return; queued
    /// for fan-out.
    pub async fn append_revoked(&self, target: GrantId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO grant_events (kind, target_id, occurred_at)
             VALUES ('revoked', ?, ?) RETURNING seq",
        )
        .bind(target as i64)
        .bind(now_millis())
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT OR IGNORE INTO projected_revocations (id) VALUES (?)")
            .bind(target as i64)
            .execute(&mut *tx)
            .await?;
        recompute_liveness(&mut tx).await?;
        set_cursor(&mut tx, seq).await?;
        tx.commit().await?;
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

/// One live grant as the projection holds it — what the token minter (M2
/// slice 4) embeds and the gate (slice 5) consults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveGrant {
    /// The grant's id (its log seq).
    pub id: GrantId,
    /// The verbs it confers.
    pub verbs: BTreeSet<Verb>,
    /// The scope it covers.
    pub scope: Scope,
}

impl SqliteGrantLog {
    /// The projection's authorization decision: may `principal` perform `verb`
    /// on `resource`? Own scope needs no grant (A1); otherwise some live grant
    /// must cover it. Must agree with [`crate::GrantModel::can`] over the
    /// replayed log — the differential property the tests enforce.
    pub async fn can(&self, principal: &Principal, verb: Verb, resource: &str) -> Result<bool> {
        if principal.owns(resource) {
            return Ok(true);
        }
        Ok(self
            .live_grants_for(principal)
            .await?
            .iter()
            .any(|grant| grant.verbs.contains(&verb) && grant.scope.covers(resource)))
    }

    /// Every currently-live grant held by `principal`, in grant order.
    pub async fn live_grants_for(&self, principal: &Principal) -> Result<Vec<LiveGrant>> {
        let Principal::Agent(agent) = principal;
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT id, verbs, scope_kind, scope_value FROM projected_grants
             WHERE grantee_agent = ? AND live = 1 ORDER BY id",
        )
        .bind(agent)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, verbs, scope_kind, scope_value)| {
                Ok(LiveGrant {
                    id: id as GrantId,
                    verbs: decode_verbs(&verbs, id)?,
                    scope: decode_scope(&scope_kind, scope_value, id)?,
                })
            })
            .collect()
    }

    /// Re-derive the whole projection from the log (wipe + replay + recompute),
    /// atomically. The recovery path for a corrupted or lost projection — the
    /// log stays untouched.
    pub async fn rebuild_projection(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM projected_grants")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM projected_revocations")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE projection_cursor SET applied_seq = 0 WHERE id = 1")
            .execute(&mut *tx)
            .await?;
        apply_events_after(&mut tx, 0).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Apply any log events beyond the projection cursor (idempotent; a no-op
    /// when level). Runs at `open`.
    async fn catch_up(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let cursor: i64 =
            sqlx::query_scalar("SELECT applied_seq FROM projection_cursor WHERE id = 1")
                .fetch_one(&mut *tx)
                .await?;
        apply_events_after(&mut tx, cursor).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Fold every log event with `seq > after` into the projection tables, then
/// recompute liveness and advance the cursor. Liveness is a pure function of
/// the final grant/revocation sets, so one recompute after the batch is exact.
async fn apply_events_after(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    after: i64,
) -> Result<()> {
    let max_seq: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM grant_events")
        .fetch_one(&mut **tx)
        .await?;
    let Some(max_seq) = max_seq else {
        return Ok(());
    };
    if max_seq <= after {
        return Ok(());
    }
    sqlx::query(
        "INSERT OR IGNORE INTO projected_grants
             (id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value)
         SELECT seq, grantor_agent, grantee_agent, verbs, scope_kind, scope_value
         FROM grant_events WHERE seq > ? AND kind = 'granted'",
    )
    .bind(after)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO projected_revocations (id)
         SELECT target_id FROM grant_events
         WHERE seq > ? AND kind = 'revoked' AND target_id IS NOT NULL",
    )
    .bind(after)
    .execute(&mut **tx)
    .await?;
    recompute_liveness(tx).await?;
    set_cursor(tx, max_seq).await?;
    Ok(())
}

/// Recompute every grant's cached liveness — the projection's copy of the
/// model's chain rule: unrevoked, and (for a delegation) backed by an
/// **earlier, live** grant giving the grantor `Grant` over a covering scope
/// with superset verbs. A single forward pass in id order is exact, because a
/// grant's liveness depends only on strictly earlier grants.
async fn recompute_liveness(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<()> {
    use std::collections::HashSet;
    let revoked: HashSet<i64> = sqlx::query_scalar("SELECT id FROM projected_revocations")
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .collect();
    // (id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, live)
    type ProjectedTuple = (i64, Option<String>, String, String, String, String, i64);
    let rows: Vec<ProjectedTuple> = sqlx::query_as(
        "SELECT id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, live
         FROM projected_grants ORDER BY id",
    )
    .fetch_all(&mut **tx)
    .await?;

    struct Decoded {
        id: i64,
        grantor_agent: Option<String>,
        grantee_agent: String,
        verbs: BTreeSet<Verb>,
        scope: Scope,
        was_live: bool,
        live: bool,
    }
    let mut grants = Vec::with_capacity(rows.len());
    for (id, grantor_agent, grantee_agent, verbs, scope_kind, scope_value, live) in rows {
        grants.push(Decoded {
            id,
            grantor_agent,
            grantee_agent,
            verbs: decode_verbs(&verbs, id)?,
            scope: decode_scope(&scope_kind, scope_value, id)?,
            was_live: live != 0,
            live: false,
        });
    }
    for i in 0..grants.len() {
        let live = !revoked.contains(&grants[i].id)
            && match &grants[i].grantor_agent {
                None => true,
                Some(delegator) => grants[..i].iter().any(|sup| {
                    sup.live
                        && sup.grantee_agent == *delegator
                        && sup.verbs.contains(&Verb::Grant)
                        && sup.verbs.is_superset(&grants[i].verbs)
                        && sup.scope.covers_scope(&grants[i].scope)
                }),
            };
        grants[i].live = live;
    }
    for grant in &grants {
        if grant.live != grant.was_live {
            sqlx::query("UPDATE projected_grants SET live = ? WHERE id = ?")
                .bind(grant.live as i64)
                .bind(grant.id)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

/// Advance the projection cursor to `seq` (monotone: never moves backwards).
async fn set_cursor(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, seq: i64) -> Result<()> {
    sqlx::query("UPDATE projection_cursor SET applied_seq = ? WHERE id = 1 AND applied_seq < ?")
        .bind(seq)
        .bind(seq)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Decode a stored verbs JSON array; a malformed value is corruption.
fn decode_verbs(json: &str, id: i64) -> Result<BTreeSet<Verb>> {
    serde_json::from_str(json)
        .map_err(|_| StoreError::Corrupt(format!("projected grant {id}: invalid verbs")))
}

/// Decode a stored (scope_kind, scope_value) pair; a malformed kind is
/// corruption.
fn decode_scope(kind: &str, value: String, id: i64) -> Result<Scope> {
    match kind {
        "name" => Ok(Scope::Name(value)),
        "namespace" => Ok(Scope::Namespace(value)),
        _ => Err(StoreError::Corrupt(format!(
            "projected grant {id}: invalid scope kind {kind:?}"
        ))),
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::grants::GrantModel;
    use crate::grants::test_strategies::{AGENTS, RESOURCES, arb_grantor, arb_scope, arb_verbs};
    use proptest::prelude::*;

    async fn log() -> (tempfile::TempDir, SqliteGrantLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = SqliteGrantLog::open(dir.path().join("grants.db"))
            .await
            .unwrap();
        (dir, log)
    }

    fn alice() -> Principal {
        Principal::Agent("alice".into())
    }

    fn bob() -> Principal {
        Principal::Agent("bob".into())
    }

    /// The store's decisions over the shared query grid.
    async fn store_decisions(log: &SqliteGrantLog) -> Vec<bool> {
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

    /// The model's decisions over the same grid, from the log's own replay.
    fn model_decisions(model: &GrantModel) -> Vec<bool> {
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

    #[tokio::test]
    async fn projection_tracks_grant_and_revocation() {
        let (_d, log) = log().await;
        assert!(
            !log.can(&alice(), Verb::Read, "research.papers.doc1")
                .await
                .unwrap()
        );
        let id = log
            .append_granted(
                &Grantor::Operator,
                &alice(),
                &BTreeSet::from([Verb::Read]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        assert!(
            log.can(&alice(), Verb::Read, "research.papers.doc1")
                .await
                .unwrap()
        );
        assert!(
            !log.can(&alice(), Verb::Write, "research.papers.doc1")
                .await
                .unwrap()
        );

        log.append_revoked(id).await.unwrap();
        assert!(
            !log.can(&alice(), Verb::Read, "research.papers.doc1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn upstream_revocation_cascades_through_the_projection() {
        let (_d, log) = log().await;
        let root = log
            .append_granted(
                &Grantor::Operator,
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Grant]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        log.append_granted(
            &Grantor::Agent("alice".into()),
            &bob(),
            &BTreeSet::from([Verb::Read]),
            &Scope::Namespace("research.papers".into()),
        )
        .await
        .unwrap();
        assert!(
            log.can(&bob(), Verb::Read, "research.papers.doc1")
                .await
                .unwrap()
        );

        log.append_revoked(root).await.unwrap();
        assert!(
            !log.can(&bob(), Verb::Read, "research.papers.doc1")
                .await
                .unwrap(),
            "the delegated subtree dies with its support (A3)"
        );
        assert!(log.live_grants_for(&bob()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn own_scope_needs_no_grant() {
        let (_d, log) = log().await;
        assert!(
            log.can(&alice(), Verb::Write, "system.agents.alice.files.notes")
                .await
                .unwrap()
        );
        assert!(
            !log.can(&alice(), Verb::Read, "system.agents.bob.files.x")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn rebuild_recovers_a_corrupted_projection() {
        let (_d, log) = log().await;
        let id = log
            .append_granted(
                &Grantor::Operator,
                &alice(),
                &BTreeSet::from([Verb::Read, Verb::Grant]),
                &Scope::Namespace("research".into()),
            )
            .await
            .unwrap();
        log.append_granted(
            &Grantor::Agent("alice".into()),
            &bob(),
            &BTreeSet::from([Verb::Read]),
            &Scope::Namespace("research.papers".into()),
        )
        .await
        .unwrap();
        log.append_revoked(id).await.unwrap();
        let truth = store_decisions(&log).await;

        // Corrupt the derived state: flip every liveness flag and drop a row.
        sqlx::query("UPDATE projected_grants SET live = 1 - live")
            .execute(&log.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM projected_revocations")
            .execute(&log.pool)
            .await
            .unwrap();
        assert_ne!(store_decisions(&log).await, truth, "corruption is visible");

        // Rebuild from the log restores exactly the pre-corruption decisions.
        log.rebuild_projection().await.unwrap();
        assert_eq!(store_decisions(&log).await, truth);
    }

    #[tokio::test]
    async fn open_catches_up_a_stale_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.db");
        let log = SqliteGrantLog::open(&path).await.unwrap();
        log.append_granted(
            &Grantor::Operator,
            &alice(),
            &BTreeSet::from([Verb::Read]),
            &Scope::Namespace("research".into()),
        )
        .await
        .unwrap();
        let truth = store_decisions(&log).await;

        // Simulate a projection that lags the log (e.g. written by a binary
        // that predates the projection): wipe it and reset the cursor.
        sqlx::query("DELETE FROM projected_grants")
            .execute(&log.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE projection_cursor SET applied_seq = 0")
            .execute(&log.pool)
            .await
            .unwrap();
        drop(log);

        // Reopening catches up from the cursor: decisions are back.
        let log = SqliteGrantLog::open(&path).await.unwrap();
        assert_eq!(store_decisions(&log).await, truth);
    }

    /// The A5/A3 differential: after any sequence of appends, the projection's
    /// decisions equal the reference model's over the replayed log — and stay
    /// equal after a full rebuild.
    #[test]
    fn projection_agrees_with_the_model_on_arbitrary_sequences() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let strategy = proptest::collection::vec(
            (
                arb_grantor(),
                proptest::sample::select(AGENTS),
                arb_verbs(),
                arb_scope(),
                any::<bool>(),
                1u64..10,
            ),
            0..10,
        );
        let mut runner =
            proptest::test_runner::TestRunner::new(proptest::test_runner::Config::with_cases(24));
        runner
            .run(&strategy, |rows| {
                rt.block_on(async {
                    let (_d, log) = log().await;
                    for (grantor, grantee, verbs, scope, revoke, target) in rows {
                        if revoke {
                            log.append_revoked(target).await.unwrap();
                        } else {
                            log.append_granted(
                                &grantor,
                                &Principal::Agent(grantee.into()),
                                &verbs,
                                &scope,
                            )
                            .await
                            .unwrap();
                        }
                    }
                    let model = GrantModel::replay(&log.replay().await.unwrap());
                    let expected = model_decisions(&model);
                    assert_eq!(store_decisions(&log).await, expected, "projection ≡ model");
                    log.rebuild_projection().await.unwrap();
                    assert_eq!(store_decisions(&log).await, expected, "rebuild ≡ model");
                });
                Ok(())
            })
            .unwrap();
    }
}
