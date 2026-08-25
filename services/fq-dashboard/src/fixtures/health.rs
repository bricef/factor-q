//! The health page's fixtures: the two Control reports it composes.
//!
//! Their own module because the page now takes two values where it
//! took one, and `fixtures.rs` was within a few lines of the file-size
//! cap — the ratchet's answer to a file that grows is to split it, not
//! to raise its budget.
//!
//! The numbers are the ones the single `HealthReport` fixture carried,
//! redistributed to the reports that actually answer them:
//! `event_count` is `control.status`'s `projection_rows`, and
//! `executions` / `failures` belong to `control.doctor`. A screenshot
//! diff should therefore show nothing but the version line.

use fq_ops::health::{ConsumerHealth, StreamHealth};
use fq_ops::surface::{
    DoctorDeadLetters, DoctorExecutions, DoctorFailure, DoctorReport, DoctorWorkers,
    StatusRegistry, StatusReport,
};
use fq_ops::views::RecoveryView;

/// `control.status` — what the daemon is and what its streams are
/// doing.
pub(crate) fn status_report() -> StatusReport {
    StatusReport {
        version: "0.1.0+abc123def456".to_string(),
        drain_deadline_ms: 180_000,
        // The daemon reports where its stores are; a fixture stands
        // in for a daemon, so it answers too.
        stores: fq_ops::surface::StatusStores {
            worker_path: "/var/lib/factor-q/worker.db".to_string(),
            control_plane_path: "/var/lib/factor-q/control-plane.db".to_string(),
            projection_path: "/var/lib/factor-q/projection.db".to_string(),
            legacy_events_db: None,
            initialised: true,
        },
        streams: vec![
            StreamHealth::Available {
                stream: "fq-events".to_string(),
                messages: 60_744,
                bytes: 393_248_768,
                first_seq: 1,
                last_seq: 60_744,
                consumer: ConsumerHealth::Active {
                    name: "fq-projector".to_string(),
                    delivered: 60_744,
                    lag: 0,
                    ack_pending: 0,
                    num_pending: 0,
                    num_redelivered: 0,
                },
            },
            StreamHealth::Available {
                stream: "fq-triggers".to_string(),
                messages: 3,
                bytes: 333,
                first_seq: 30,
                last_seq: 32,
                consumer: ConsumerHealth::Active {
                    name: "fq-dispatcher".to_string(),
                    delivered: 29,
                    lag: 3,
                    ack_pending: 1,
                    num_pending: 2,
                    num_redelivered: 4,
                },
            },
        ],
        registry: StatusRegistry {
            agents: 4,
            load_errors: Vec::new(),
        },
        projection_rows: 64_016,
        recovery: RecoveryView {
            ambiguous: 3,
            stale_workers: 2,
            stale_worker_ids: vec!["019f3383-d8a5".to_string(), "019f339a-9613".to_string()],
        },
    }
}

/// `control.doctor` — whether anything needs an operator.
pub(crate) fn doctor_report() -> DoctorReport {
    DoctorReport {
        workers: DoctorWorkers {
            alive: 1,
            stale: 2,
            shutdown: 0,
            stale_ids: vec!["019f3383-d8a5".to_string(), "019f339a-9613".to_string()],
        },
        executions: DoctorExecutions {
            in_flight: 2,
            working: 1,
            working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
            stuck: 1,
            stuck_ids: vec!["019f534f-4b3c-7f42-a619-b5e43a64fd38".to_string()],
        },
        ambiguous: 3,
        failures: vec![
            DoctorFailure {
                error_kind: "budgetexceeded".to_string(),
                count: 2,
            },
            DoctorFailure {
                error_kind: "toolerror".to_string(),
                count: 1,
            },
            DoctorFailure {
                error_kind: "triggerexhausted".to_string(),
                count: 1,
            },
        ],
        dead_letters: DoctorDeadLetters {
            exhausted_triggers: 1,
        },
    }
}
