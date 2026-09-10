//! Rebuilding the projection under a running daemon: the durable reset
//! that turns a recreated file into a replay, the supervisor that runs
//! the consumer so it can be stopped and started around a rebuild, and
//! the handle `control.projection_rebuild` asks.
//!
//! The store's own `rebuild` module does the SQL half — drop, recreate,
//! carry the sweep-exempt rows across, stamp the version, note that the
//! durable must be reset. What that half cannot do is touch the bus.
//! This module does: [`reset_projection_consumer`] deletes the
//! `fq-projector` durable so the next consumer start creates it afresh
//! from the beginning of the stream, and records the stream's last
//! sequence as the replay's target.
//!
//! **A reset is never performed under a running consumer loop.** The
//! loop's message stream would end, the daemon supervises that as a
//! task failure, and the process would come down — which is exactly
//! how deleting a durable by hand behaves today, and why the
//! [`ProjectionSupervisor`] exists. It owns the consumer task: on a
//! rebuild request it stops the loop, rebuilds the file, resets the
//! durable, and starts the loop again, and only *its* exit is what the
//! daemon supervises.
//!
//! The consumer's own parse-and-ack policy is untouched here. What an
//! unsupported envelope version does to a replay is that policy's
//! concern, and its fix; a rebuild replays whatever the consumer would
//! have projected live.

use std::sync::Arc;

use async_nats::jetstream::stream::ConsumerErrorKind;
use tokio::sync::{mpsc, oneshot};
use tracing::info;

use super::consumer::{CONSUMER_NAME, ConsumerError, ProjectionConsumer};
use super::store::{ProjectionStore, StoreError};
use crate::bus::{EventBus, STREAM_NAME};
use crate::watermark::WatermarkSender;

pub mod floor;

/// What resetting the durable found and recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerReset {
    /// Whether a durable existed to delete. A fresh file on a fresh
    /// broker finds none, and that is not a rebuild.
    pub deleted: bool,
    /// The stream's last sequence at the reset — what the replay has
    /// to reach before every event the stream holds is back.
    pub target_seq: u64,
}

fn stream_error(err: impl std::fmt::Display) -> ConsumerError {
    ConsumerError::Stream(err.to_string())
}

/// Delete the projection's durable consumer so the next start creates
/// it afresh — `DeliverAll` from the beginning of the stream — and
/// record the reset on the store.
///
/// Tolerates an absent durable. Must not be called while a consumer
/// loop is reading the durable; see the module docs.
pub async fn reset_projection_consumer(
    bus: &EventBus,
    store: &ProjectionStore,
) -> Result<ConsumerReset, ConsumerError> {
    let mut stream = bus
        .jetstream()
        .get_stream(STREAM_NAME)
        .await
        .map_err(stream_error)?;
    let deleted = match stream.delete_consumer(CONSUMER_NAME).await {
        Ok(status) => status.success,
        Err(err) if matches!(err.kind(), ConsumerErrorKind::JetStream(js) if js.code() == 404) => {
            false
        }
        Err(err) => return Err(stream_error(err)),
    };
    let target_seq = stream
        .info()
        .await
        .map_err(stream_error)?
        .state
        .last_sequence;
    store.consumer_reset_done(target_seq, deleted).await?;
    info!(
        consumer = CONSUMER_NAME,
        deleted,
        target_seq,
        "projection durable reset; the next consumer start replays the stream from the beginning"
    );
    Ok(ConsumerReset {
        deleted,
        target_seq,
    })
}

/// The stream position the projector has acked up to, or `None` when
/// the durable cannot be read (it is between deletion and recreation,
/// or the broker is away).
async fn projector_ack_floor(bus: &EventBus) -> Option<u64> {
    let stream = bus.jetstream().get_stream(STREAM_NAME).await.ok()?;
    let info = stream.consumer_info(CONSUMER_NAME).await.ok()?;
    Some(info.ack_floor.stream_sequence)
}

/// The projection's last rebuild as `control.status` reports it, or
/// `None` for a projection that has never been rebuilt. Judged against
/// the durable's acked floor now, so "in progress" is a live reading.
pub async fn rebuild_status(
    store: &ProjectionStore,
    bus: &EventBus,
) -> Result<Option<fq_ops::surface::ProjectionRebuild>, StoreError> {
    let Some(record) = store.rebuild_record().await? else {
        return Ok(None);
    };
    let pending = store.consumer_reset_pending().await?.is_some();
    let floor = if pending {
        None
    } else {
        projector_ack_floor(bus).await
    };
    Ok(Some(record.status(pending, floor)))
}

/// Why an operator's rebuild request could not be honoured.
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    /// No supervisor is running to stop and restart the consumer — the
    /// handle is detached, or the daemon is already shutting down.
    #[error("no projection supervisor is running to rebuild the projection")]
    NoSupervisor,
    #[error("projection store: {0}")]
    Store(#[from] StoreError),
    #[error("projection consumer: {0}")]
    Consumer(#[from] ConsumerError),
}

/// One operator request: why, and where to send the answer.
struct RebuildRequest {
    reason: Option<String>,
    reply: oneshot::Sender<Result<fq_ops::surface::ProjectionRebuild, RebuildError>>,
}

/// The operator's way to ask the running supervisor for a rebuild.
/// Cheap to clone; every clone reaches the same supervisor.
#[derive(Clone)]
pub struct ProjectionRebuildHandle {
    requests: mpsc::Sender<RebuildRequest>,
}

impl ProjectionRebuildHandle {
    /// A handle with nothing behind it: every request fails with
    /// [`RebuildError::NoSupervisor`]. For assembling an operator
    /// surface whose declarations are read but never invoked.
    pub fn detached() -> Self {
        let (requests, _dropped) = mpsc::channel(1);
        Self { requests }
    }

    /// Rebuild the projection: stop the consumer, drop and recreate the
    /// tables, reset the durable, start the consumer again. Answers
    /// with the rebuild as `control.status` will report it — the replay
    /// has begun, not finished.
    pub async fn rebuild(
        &self,
        reason: Option<String>,
    ) -> Result<fq_ops::surface::ProjectionRebuild, RebuildError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(RebuildRequest { reason, reply })
            .await
            .map_err(|_| RebuildError::NoSupervisor)?;
        answer.await.map_err(|_| RebuildError::NoSupervisor)?
    }
}

/// Runs the projection consumer for the life of the daemon, stopping
/// and restarting it around operator rebuilds. The daemon supervises
/// this task where it used to supervise the consumer directly: it
/// exits when told to, or when the consumer it runs stops on its own.
pub struct ProjectionSupervisor {
    bus: EventBus,
    store: Arc<ProjectionStore>,
    watermark: Option<WatermarkSender>,
    handle: ProjectionRebuildHandle,
    requests: mpsc::Receiver<RebuildRequest>,
}

impl ProjectionSupervisor {
    pub fn new(bus: EventBus, store: Arc<ProjectionStore>) -> Self {
        // One request at a time: a rebuild is not something to queue
        // up, and a second caller waits on the send until the first is
        // answered.
        let (requests_tx, requests) = mpsc::channel(1);
        Self {
            bus,
            store,
            watermark: None,
            handle: ProjectionRebuildHandle {
                requests: requests_tx,
            },
            requests,
        }
    }

    /// Advance `watermark` as events apply — handed to every consumer
    /// this supervisor starts, so the mark survives a rebuild. It is
    /// monotonic, so a replay from the beginning never regresses it;
    /// until the replay catches up, a read released at the old mark
    /// can find its row not yet re-derived.
    pub fn with_watermark(mut self, watermark: WatermarkSender) -> Self {
        self.watermark = Some(watermark);
        self
    }

    /// The handle `control.projection_rebuild` holds.
    pub fn rebuild_handle(&self) -> ProjectionRebuildHandle {
        self.handle.clone()
    }

    fn consumer(&self) -> ProjectionConsumer {
        let consumer = ProjectionConsumer::new(self.bus.clone(), self.store.clone());
        match &self.watermark {
            Some(watermark) => consumer.with_watermark(watermark.clone()),
            None => consumer,
        }
    }

    /// Run until `shutdown` fires or the consumer stops on its own.
    pub async fn run(mut self, mut shutdown: oneshot::Receiver<()>) -> Result<(), ConsumerError> {
        // Once every handle is dropped there is nobody to ask for a
        // rebuild; the receiver would answer `None` forever, so it is
        // taken out of the select rather than polled.
        let mut closed = false;
        loop {
            let (stop_tx, stop_rx) = oneshot::channel();
            let mut task = tokio::spawn(self.consumer().run(stop_rx));
            let request = loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        let _ = stop_tx.send(());
                        return join_consumer(task).await;
                    }
                    result = &mut task => return join_result(result),
                    request = self.requests.recv(), if !closed => match request {
                        Some(request) => break request,
                        None => closed = true,
                    }
                }
            };
            let _ = stop_tx.send(());
            join_consumer(task).await?;
            let outcome = self.rebuild(request.reason.as_deref()).await;
            // A caller that stopped waiting still gets its rebuild: the
            // file is already rebuilt by the time the answer is sent.
            let _ = request.reply.send(outcome);
        }
    }

    async fn rebuild(
        &self,
        reason: Option<&str>,
    ) -> Result<fq_ops::surface::ProjectionRebuild, RebuildError> {
        self.store.rebuild(reason).await?;
        reset_projection_consumer(&self.bus, &self.store).await?;
        rebuild_status(&self.store, &self.bus)
            .await?
            .ok_or_else(|| {
                RebuildError::Store(StoreError::Backend(
                    "the rebuild left no record on the projection".to_string(),
                ))
            })
    }
}

/// The consumer's verdict once it has been asked to stop.
async fn join_consumer(
    task: tokio::task::JoinHandle<Result<(), ConsumerError>>,
) -> Result<(), ConsumerError> {
    join_result(task.await)
}

/// A consumer panic is a panic here too: the daemon's supervision reads
/// a task that panicked differently from one that returned an error,
/// and the supervisor must not launder one into the other.
fn join_result(
    result: Result<Result<(), ConsumerError>, tokio::task::JoinError>,
) -> Result<(), ConsumerError> {
    match result {
        Ok(verdict) => verdict,
        Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
        Err(join) => Err(ConsumerError::Stream(format!(
            "projection consumer task was cancelled: {join}"
        ))),
    }
}

#[cfg(test)]
mod tests;
