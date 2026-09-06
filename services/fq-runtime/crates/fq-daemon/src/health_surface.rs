//! `control.doctor` and `control.status`, registered together.
//!
//! Their own module because they are one question between them — is the
//! machinery alright — and because both grew the same two new
//! dependencies when health started covering every durable (#549): the
//! daemon's broker connection, since the consumer probe is a JetStream
//! read only this process can make, and the facts about the daemon a
//! reader cannot derive. The registration lived inline in
//! `operator_surface`, which is at its size cap; new code goes in a new
//! module rather than into a file that may only shrink.

use std::sync::Arc;

use fq_runtime::views::Views;

use crate::operator_surface::DaemonFacts;

/// Register the two health composites together.
///
/// One call because they answer one question between them — is the
/// machinery alright — and because both now need the same three things:
/// the read views, the daemon's own broker connection (the consumer
/// probe is a JetStream read, and only this process holds one), and the
/// facts about the daemon that a reader cannot derive. Grouping them
/// also keeps `operator_registry` under the function-size gate, which
/// the second report's extra arguments had pushed it past.
///
/// The pairs are (doctor, status) — two handles of the same thing,
/// because each registration takes ownership of its own clone.
pub(crate) fn register_health_reports(
    registry: &mut fq_edge::EdgeRegistry,
    views: (Arc<Views>, Arc<Views>),
    buses: (fq_runtime::EventBus, fq_runtime::EventBus),
    agents: fq_runtime::SharedRegistry,
    facts: &DaemonFacts,
) -> anyhow::Result<()> {
    let (doctor_views, status_views) = views;
    let (doctor_bus, status_bus) = buses;
    crate::doctor_report::register_doctor_report(
        registry,
        doctor_views,
        doctor_bus,
        facts.stuck_after_ms,
        facts.summary_enabled,
    )?;
    crate::status_report::register_status_report(registry, status_views, status_bus, agents, facts)
}
