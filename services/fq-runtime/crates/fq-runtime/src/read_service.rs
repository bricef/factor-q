//! The runtime's read-only operator service — a `tarpc` wire-mirror of
//! [`crate::views`] plus the [`crate::health`] JetStream probe (#105
//! layer 2), following the same discipline as `fq-store`'s
//! `CasService`: a wire trait, a serializable `WireError`, a handler
//! forwarding to the backing library, and a `bind`/`serve` split so
//! tests can learn the ephemeral address.
//!
//! **Localhost-only and unauthenticated**, matching the runtime's
//! current posture (NATS unauthenticated on loopback, `fq-cas serve`
//! localhost-only until M5): [`bind`] refuses a non-loopback address
//! outright. Remote exposure is gated on the same broader auth work.
//!
//! The service runs in-daemon on its own tokio task, read-only against
//! the stores (`?mode=ro`) and sharing the daemon's NATS connection for
//! the probe. All browser-shaped complexity (HTTP, pagination,
//! rendering) belongs to the separate `fq-dashboard` process, not here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{StreamExt, future};
use serde::{Deserialize, Serialize};
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;
use tarpc::{client, context};

use crate::control_plane::store::OwnerStatus;
use crate::health::{self, StreamHealth};
use crate::views::{
    ActiveInvocationView, AgentCostDetailView, CostReport, EventView, ExecutionsView, FailureView,
    InvocationDetailView, InvocationSummaryView, RecoveryView, Views, ViewsError, WorkerDetailView,
    WorkerView,
};

/// Staleness / stuck-ness threshold used by [`ReadService::health`] —
/// the control-plane's `DEFAULT_STALE_THRESHOLD_MS`, the same value
/// `fq status` and `fq doctor` report with.
const HEALTH_THRESHOLD_MS: i64 = 30_000;

/// A serializable error carried over the wire ([`ViewsError`] wraps
/// store errors that aren't). Reads have no domain error cases worth
/// distinguishing yet, so the shape is a flat message.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum WireError {
    #[error("{0}")]
    Message(String),
}

impl From<ViewsError> for WireError {
    fn from(e: ViewsError) -> Self {
        WireError::Message(e.to_string())
    }
}

/// The composed health view: the NATS-side stream probe plus the
/// DB-side counts — the answer to the plan's open question on
/// `HealthReport` vs `DoctorReport` (this is the wire shape; `fq
/// doctor` remains the CLI's opinionated aggregation).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HealthReport {
    /// The serving daemon's version (semver+sha), so a dashboard shows
    /// what is actually running.
    pub version: String,
    pub streams: Vec<StreamHealth>,
    pub event_count: i64,
    pub recovery: RecoveryView,
    pub executions: ExecutionsView,
    pub failures: Vec<FailureView>,
}

/// The RPC surface, mirroring [`Views`] (see the module doc for why the
/// daemon speaks tarpc here rather than HTTP).
#[tarpc::service]
pub trait ReadService {
    /// The serving daemon's version string (`semver+sha`) — identical
    /// to [`HealthReport::version`], but reachable when nothing else
    /// is. This is the dashboard's build-skew probe (#168): every
    /// other RPC returns types whose shape changes across builds, and
    /// under the length-framed binary codec a cross-build pairing
    /// fails with a decode error that renders as "runtime
    /// unreachable". **This signature is frozen forever** — a bare
    /// `String`, no struct, no `Result`/`WireError` (those can
    /// themselves change shape) — so it decodes across *any* build
    /// pair. Never extend or wrap it; add a new method instead.
    async fn version() -> String;
    async fn health() -> Result<HealthReport, WireError>;
    /// Currently-executing invocations from the worker WAL, longest-
    /// running first, with their open dispatches.
    async fn active_invocations() -> Result<Vec<ActiveInvocationView>, WireError>;
    async fn workers() -> Result<Vec<WorkerView>, WireError>;
    async fn worker(id: String) -> Result<Option<WorkerDetailView>, WireError>;
    /// `status` accepts `in_flight | ambiguous | completed | failed`.
    async fn invocations(
        status: Option<String>,
        include_archived: bool,
        limit: i64,
    ) -> Result<Vec<InvocationSummaryView>, WireError>;
    async fn invocation(id: String) -> Result<Option<InvocationDetailView>, WireError>;
    /// The payload-bearing transcript for one invocation, as its
    /// canonical JSON array of `transcript::TranscriptEntry`.
    /// JSON-in-a-String rather than typed structs on the wire because
    /// `TranscriptEntry` is internally-tagged serde, which bincode
    /// cannot carry (the `StreamHealth` lesson) — and re-tagging would
    /// break the CLI's shipped `--format json` shape. One canonical
    /// JSON shape everywhere. `truncate_bytes` caps each payload chunk
    /// server-side (`None` = full), so big transcripts don't cross the
    /// wire to render a summary page.
    async fn transcript(
        id: String,
        truncate_bytes: Option<u64>,
    ) -> Result<Option<String>, WireError>;
    /// Transcript entries strictly after index `after` — the incremental
    /// read behind the dashboard's SSE stream. The entry list is
    /// append-only and deterministically ordered, so a plain index is a
    /// safe cursor. Returns `(json_entries, next_cursor)`; `None` when
    /// the invocation has no transcript at all.
    async fn transcript_since(
        id: String,
        after: u64,
        truncate_bytes: Option<u64>,
    ) -> Result<Option<(String, u64)>, WireError>;
    async fn events(
        agent: Option<String>,
        event_type: Option<String>,
        since: Option<String>,
        limit: i64,
    ) -> Result<Vec<EventView>, WireError>;
    async fn costs(agent: Option<String>, since: Option<String>) -> Result<CostReport, WireError>;
    /// One agent's cost drill-down: totals plus per-model and
    /// per-invocation breakdowns (invocations newest first, capped at
    /// `invocation_limit`). `None` when the agent has no cost events
    /// in the window.
    async fn agent_costs(
        agent: String,
        since: Option<String>,
        invocation_limit: i64,
    ) -> Result<Option<AgentCostDetailView>, WireError>;
}

/// Server handler: forwards each RPC to the backing [`Views`] and the
/// JetStream probe.
#[derive(Clone)]
struct ReadServer {
    views: Arc<Views>,
    js: async_nats::jetstream::Context,
    probe_timeout: Duration,
    version: String,
}

fn parse_status(s: &str) -> Result<OwnerStatus, WireError> {
    match s {
        "in_flight" => Ok(OwnerStatus::InFlight),
        "ambiguous" => Ok(OwnerStatus::Ambiguous),
        "completed" => Ok(OwnerStatus::Completed),
        "failed" => Ok(OwnerStatus::Failed),
        other => Err(WireError::Message(format!(
            "unknown status filter `{other}` — try in_flight | ambiguous | completed | failed"
        ))),
    }
}

impl ReadService for ReadServer {
    async fn version(self, _: context::Context) -> String {
        self.version.clone()
    }

    async fn health(self, _: context::Context) -> Result<HealthReport, WireError> {
        // The probe is the only network-touching read; bound it so a
        // wedged JetStream cannot wedge the whole health surface.
        let streams =
            match tokio::time::timeout(self.probe_timeout, health::probe_core_streams(&self.js))
                .await
            {
                Ok(streams) => streams,
                Err(_) => health::CORE_STREAMS
                    .iter()
                    .map(|(stream, _)| StreamHealth::Unavailable {
                        stream: stream.to_string(),
                        error: format!(
                            "probe timed out after {}ms",
                            self.probe_timeout.as_millis()
                        ),
                    })
                    .collect(),
            };

        let now_ms = chrono::Utc::now().timestamp_millis();
        Ok(HealthReport {
            version: self.version.clone(),
            streams,
            event_count: self.views.event_count().await?,
            recovery: self.views.recovery(now_ms, HEALTH_THRESHOLD_MS).await?,
            executions: self
                .views
                .executions(
                    now_ms,
                    HEALTH_THRESHOLD_MS,
                    crate::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
                )
                .await?,
            failures: self.views.failures().await?,
        })
    }

    async fn active_invocations(
        self,
        _: context::Context,
    ) -> Result<Vec<ActiveInvocationView>, WireError> {
        Ok(self.views.active_invocations().await?)
    }

    async fn workers(self, _: context::Context) -> Result<Vec<WorkerView>, WireError> {
        Ok(self.views.workers().await?)
    }

    async fn worker(
        self,
        _: context::Context,
        id: String,
    ) -> Result<Option<WorkerDetailView>, WireError> {
        Ok(self.views.worker(&id).await?)
    }

    async fn invocations(
        self,
        _: context::Context,
        status: Option<String>,
        include_archived: bool,
        limit: i64,
    ) -> Result<Vec<InvocationSummaryView>, WireError> {
        let status = status.as_deref().map(parse_status).transpose()?;
        Ok(self
            .views
            .invocation_index(status, include_archived, limit)
            .await?)
    }

    async fn invocation(
        self,
        _: context::Context,
        id: String,
    ) -> Result<Option<InvocationDetailView>, WireError> {
        Ok(self.views.invocation(&id).await?)
    }

    async fn transcript(
        self,
        _: context::Context,
        id: String,
        truncate_bytes: Option<u64>,
    ) -> Result<Option<String>, WireError> {
        let Some(mut entries) = self.views.transcript(&id).await? else {
            return Ok(None);
        };
        if let Some(max) = truncate_bytes {
            crate::transcript::truncate_entries(&mut entries, max as usize);
        }
        let json = serde_json::to_string(&entries)
            .map_err(|e| WireError::Message(format!("transcript serialisation: {e}")))?;
        Ok(Some(json))
    }

    async fn transcript_since(
        self,
        _: context::Context,
        id: String,
        after: u64,
        truncate_bytes: Option<u64>,
    ) -> Result<Option<(String, u64)>, WireError> {
        let Some(entries) = self.views.transcript(&id).await? else {
            return Ok(None);
        };
        let next = entries.len() as u64;
        let start = (after as usize).min(entries.len());
        let mut fresh = entries[start..].to_vec();
        if let Some(max) = truncate_bytes {
            crate::transcript::truncate_entries(&mut fresh, max as usize);
        }
        let json = serde_json::to_string(&fresh)
            .map_err(|e| WireError::Message(format!("transcript serialisation: {e}")))?;
        Ok(Some((json, next)))
    }

    async fn events(
        self,
        _: context::Context,
        agent: Option<String>,
        event_type: Option<String>,
        since: Option<String>,
        limit: i64,
    ) -> Result<Vec<EventView>, WireError> {
        Ok(self
            .views
            .events(
                agent.as_deref(),
                event_type.as_deref(),
                since.as_deref(),
                limit,
            )
            .await?)
    }

    async fn costs(
        self,
        _: context::Context,
        agent: Option<String>,
        since: Option<String>,
    ) -> Result<CostReport, WireError> {
        Ok(self.views.costs(agent.as_deref(), since.as_deref()).await?)
    }

    async fn agent_costs(
        self,
        _: context::Context,
        agent: String,
        since: Option<String>,
        invocation_limit: i64,
    ) -> Result<Option<AgentCostDetailView>, WireError> {
        Ok(self
            .views
            .agent_costs(&agent, since.as_deref(), invocation_limit)
            .await?)
    }
}

/// Bind a TCP listener and return its address plus a future that serves
/// requests until dropped. Splitting bind from serve lets callers
/// (tests, the daemon's log line) learn the ephemeral address before
/// the server starts. Refuses a non-loopback address — see module doc.
pub async fn bind(
    addr: &str,
    views: Arc<Views>,
    js: async_nats::jetstream::Context,
    probe_timeout: Duration,
    version: String,
) -> std::io::Result<(SocketAddr, BoxFuture<'static, ()>)> {
    let requested: SocketAddr = addr.parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid read_service bind address `{addr}`: {e}"),
        )
    })?;
    if !requested.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "read_service bind `{addr}` is not loopback — the service is \
                 unauthenticated and must not be reachable off-box"
            ),
        ));
    }

    let mut listener = tarpc::serde_transport::tcp::listen(&requested, Bincode::default).await?;
    // Responses can carry tool dispatch payloads from the WAL; bound the
    // frame well under the event bus's own 16MB-class payloads but far
    // above any sane read (same DoS rationale as fq-store's bind).
    listener.config_mut().max_frame_length(64 << 20); // 64 MiB
    let local_addr = listener.local_addr();
    let serving: BoxFuture<'static, ()> = Box::pin(async move {
        listener
            .filter_map(|r| future::ready(r.ok()))
            .map(BaseChannel::with_defaults)
            .for_each_concurrent(None, move |channel| {
                let server = ReadServer {
                    views: views.clone(),
                    js: js.clone(),
                    probe_timeout,
                    version: version.clone(),
                };
                channel
                    .execute(server.serve())
                    .for_each(|response| async move {
                        tokio::spawn(response);
                    })
            })
            .await;
    });
    Ok((local_addr, serving))
}

/// Connect a [`ReadServiceClient`] to a read service at `addr`
/// (e.g. "127.0.0.1:9471").
pub async fn connect(addr: &str) -> std::io::Result<ReadServiceClient> {
    let transport = tarpc::serde_transport::tcp::connect(addr, Bincode::default).await?;
    Ok(ReadServiceClient::new(client::Config::default(), transport).spawn())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::projection::ProjectionStore;
    use crate::control_plane::store::ControlPlaneStore;
    use crate::worker::store::WorkerStore;

    /// Round-trip the DB-backed reads over a real ephemeral TCP wire:
    /// seed a temp DB, bind on 127.0.0.1:0, and read back through the
    /// generated client — the same shape as fq-store's conformance run
    /// against `RemoteStore`. The NATS probe is exercised separately
    /// (it needs a broker); here the health call is not exercised.
    #[tokio::test]
    async fn db_reads_round_trip_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let cp = ControlPlaneStore::open(&path).await.unwrap();
            cp.register_worker("w1", "localhost", 100).await.unwrap();
            let _ws = WorkerStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }

        let views = Arc::new(Views::open(&path).await.unwrap());
        // The probe needs a jetstream context but health() is not called
        // here; connecting lazily means no broker is required.
        let js = async_nats::jetstream::new(
            async_nats::connect_with_options(
                "nats://127.0.0.1:1", // never dialled in this test
                async_nats::ConnectOptions::new().retry_on_initial_connect(),
            )
            .await
            .expect("lazy client"),
        );

        let (addr, serving) = bind(
            "127.0.0.1:0",
            views,
            js,
            Duration::from_millis(100),
            "test-version".to_string(),
        )
        .await
        .expect("bind ephemeral");
        tokio::spawn(serving);

        let client = connect(&addr.to_string()).await.expect("connect");

        let workers = client
            .workers(context::current())
            .await
            .expect("rpc")
            .expect("workers");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].worker_id, "w1");

        let detail = client
            .worker(context::current(), "w1".to_string())
            .await
            .expect("rpc")
            .expect("worker");
        assert_eq!(detail.expect("w1 exists").worker.worker_id, "w1");

        let active = client
            .active_invocations(context::current())
            .await
            .expect("rpc")
            .expect("active");
        assert!(active.is_empty());

        let invocations = client
            .invocations(context::current(), None, true, 50)
            .await
            .expect("rpc")
            .expect("invocations");
        assert!(invocations.is_empty());

        // An unknown status filter surfaces as a WireError, not a hang.
        let err = client
            .invocations(context::current(), Some("bogus".into()), false, 50)
            .await
            .expect("rpc")
            .expect_err("bogus filter must be rejected");
        assert!(err.to_string().contains("bogus"));

        let events = client
            .events(context::current(), None, None, None, 50)
            .await
            .expect("rpc")
            .expect("events");
        assert!(events.is_empty());

        let costs = client
            .costs(context::current(), None, None)
            .await
            .expect("rpc")
            .expect("costs");
        assert!(costs.agents.is_empty());

        // The drill-down round-trips its None case for an unknown agent.
        let detail = client
            .agent_costs(context::current(), "no-such-agent".to_string(), None, 10)
            .await
            .expect("rpc")
            .expect("agent_costs");
        assert!(detail.is_none());

        let missing = client
            .invocation(context::current(), "no-such-id".to_string())
            .await
            .expect("rpc")
            .expect("invocation");
        assert!(missing.is_none());

        // Cursor reads: no transcript at all → None, whatever the cursor.
        let since = client
            .transcript_since(context::current(), "no-such-id".to_string(), 0, None)
            .await
            .expect("rpc")
            .expect("since");
        assert!(since.is_none());
    }

    /// End-to-end `health()` over the wire against a real broker —
    /// exercises the probe, its timeout wrapper, and the composed
    /// report. Self-gating: skips when no broker listens on the dev
    /// port (`just ci` and the CI runtime job both provide one).
    #[tokio::test]
    async fn health_round_trips_against_a_live_broker() {
        let Ok(bus) = crate::bus::EventBus::connect("nats://127.0.0.1:4222").await else {
            eprintln!("skipping: no NATS broker on 127.0.0.1:4222");
            return;
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let _cp = ControlPlaneStore::open(&path).await.unwrap();
            let _ws = WorkerStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }
        let views = Arc::new(Views::open(&path).await.unwrap());

        let (addr, serving) = bind(
            "127.0.0.1:0",
            views,
            bus.jetstream(),
            Duration::from_millis(2_000),
            "test-version".to_string(),
        )
        .await
        .expect("bind ephemeral");
        tokio::spawn(serving);

        let client = connect(&addr.to_string()).await.expect("connect");
        let health = client
            .health(context::current())
            .await
            .expect("rpc")
            .expect("health");

        assert_eq!(health.version, "test-version");
        // The frozen skew probe (#168) returns the same string bare —
        // reachable even when shape-carrying RPCs would not decode.
        let version = client.version(context::current()).await.expect("rpc");
        assert_eq!(version, "test-version");
        // `EventBus::connect` ensured both core streams exist, so the
        // probe must report them Available. Their primary consumers may
        // or may not exist on the shared dev broker — both states are
        // valid here, so only the stream level is asserted.
        assert_eq!(health.streams.len(), 2);
        for s in &health.streams {
            assert!(
                matches!(s, StreamHealth::Available { .. }),
                "expected Available, got {s:?}"
            );
        }
        assert_eq!(health.event_count, 0);
    }

    /// A non-loopback bind is refused outright — the service is
    /// unauthenticated; never reachable off-box.
    #[tokio::test]
    async fn refuses_non_loopback_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let _cp = ControlPlaneStore::open(&path).await.unwrap();
            let _ws = WorkerStore::open(&path).await.unwrap();
            let _proj = ProjectionStore::open(&path).await.unwrap();
        }
        let views = Arc::new(Views::open(&path).await.unwrap());
        let js = async_nats::jetstream::new(
            async_nats::connect_with_options(
                "nats://127.0.0.1:1",
                async_nats::ConnectOptions::new().retry_on_initial_connect(),
            )
            .await
            .expect("lazy client"),
        );

        let Err(err) = bind(
            "0.0.0.0:0",
            views,
            js,
            Duration::from_millis(100),
            "test".to_string(),
        )
        .await
        else {
            panic!("0.0.0.0 must be refused");
        };
        assert!(err.to_string().contains("loopback"), "got: {err}");
    }
}
