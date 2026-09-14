//! The hosted maintenance consumer (#257): how it is started, and its
//! on/off switch.
//!
//! Both live here rather than in `run_hosted` for two reasons. The
//! first is the ratchet: `run_hosted` is pinned at its size budget and
//! may only shrink, so a new hosted task arrives as a `spawn` call and
//! a supervision arm, not as another block of construction. The second
//! is the switch. `[maintenance] enabled` decides whether this daemon
//! runs the housekeeping tasks a scheduler publishes, and keeping the
//! decision inside the spawned task means the supervised set has one
//! shape whatever the config says: the task is always spawned, always
//! joined, and always reports a clean stop — a disabled daemon simply
//! parks until shutdown instead of creating a durable.
//!
//! The alternative — an `Option<JoinHandle>` and a "did the select arm
//! already take it" flag, as the summariser needs — buys nothing here.
//! The summariser is optional because a model must be *named*; the
//! maintenance consumer is on by default and the off case is rare, so
//! the cheap uniform shape is the right one.

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;
use uuid::Uuid;

use fq_runtime::control_plane::maintenance::{
    MaintenanceConsumer, MaintenanceConsumerError, MaintenanceContext,
};
use fq_runtime::{Config, EventBus, PricingRefresh};

/// Spawn the maintenance consumer, returning its shutdown channel and
/// its supervised handle.
///
/// `pricing` is what `pricing_refresh` runs against (#344): the settings
/// and the cache the startup load used, and the handle on the table the
/// daemon is serving. It arrives here rather than being built here
/// because the served table is assembled with the rest of the runtime,
/// before any task is spawned.
pub(crate) fn spawn(
    bus: &EventBus,
    runtime_id: Uuid,
    config: &Config,
    pricing: PricingRefresh,
) -> (
    oneshot::Sender<()>,
    JoinHandle<Result<(), MaintenanceConsumerError>>,
) {
    let consumer = MaintenanceConsumer::new(bus.clone(), runtime_id, config.maintenance.ack_wait())
        .with_context(MaintenanceContext::new().with_pricing(pricing));
    let enabled = config.maintenance.enabled;
    let (tx, rx) = oneshot::channel();
    (
        tx,
        tokio::spawn(async move { run(consumer, enabled, rx).await }),
    )
}

/// Run the consumer, or park until shutdown when maintenance is off.
async fn run(
    consumer: MaintenanceConsumer,
    enabled: bool,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), MaintenanceConsumerError> {
    if !enabled {
        info!(
            "maintenance consumer disabled by `[maintenance] enabled = false`; \
             scheduled maintenance published to fq.maintenance.> will age out unrun"
        );
        let _ = shutdown.await;
        return Ok(());
    }
    consumer.run(shutdown).await
}
