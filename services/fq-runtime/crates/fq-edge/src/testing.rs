//! Test support for consumers of the edge and the contract crate: a
//! **mock domain service** — the `fq_ops::fixtures` catalogue bound
//! to stateful in-memory handlers — plus a one-call server harness.
//! Consumers (daemon wiring, dashboard re-point, wrapper codegen, MCP
//! face) test against observable behaviour: drop an invocation, then
//! Get it and see the phase change.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fq_ops::fixtures::{
    ControlState, DownInput, DropInput, InvocationKey, InvocationState, PublishInput, control,
    control_down, cost_summary, invocation, invocation_drop, trigger_publish,
};
use fq_ops::{AtomRef, Domain, Receipt};

use crate::auth::EdgeIdentity;
use crate::registry::EdgeRegistry;
use crate::wire::WireError;

/// The mock domain's shared state: a seeded invocation store and an
/// advancing event sequence. Clone freely — handlers and assertions
/// share the same state.
#[derive(Clone, Default)]
pub struct MockDomain {
    invocations: Arc<Mutex<BTreeMap<String, InvocationState>>>,
    seq: Arc<AtomicU64>,
}

impl MockDomain {
    /// Two seeded invocations: `inv-1` running, `inv-2` completed.
    pub fn seeded() -> Self {
        let domain = MockDomain::default();
        {
            let mut invocations = domain.invocations.lock().unwrap();
            for (id, phase) in [("inv-1", "running"), ("inv-2", "completed")] {
                invocations.insert(
                    id.to_string(),
                    InvocationState {
                        invocation_id: id.to_string(),
                        agent_id: "researcher".to_string(),
                        phase: phase.to_string(),
                    },
                );
            }
        }
        domain.seq.store(100, Ordering::SeqCst);
        domain
    }

    pub fn invocation(&self, id: &str) -> Option<InvocationState> {
        self.invocations.lock().unwrap().get(id).cloned()
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Build the edge registry for this mock: the fixture catalogue
    /// bound to handlers over this state.
    pub fn registry(&self) -> EdgeRegistry {
        let mut registry = EdgeRegistry::new();

        let state = self.clone();
        registry
            .view::<InvocationKey, InvocationState, _, _, _, _>(
                invocation(),
                move |key| {
                    let state = state.clone();
                    async move {
                        state.invocation(&key.invocation_id).ok_or_else(|| {
                            WireError::InvalidInput {
                                op: "invocation.get".into(),
                                message: format!("no invocation `{}`", key.invocation_id),
                            }
                        })
                    }
                },
                {
                    let state = self.clone();
                    move |_filter| {
                        let state = state.clone();
                        async move {
                            let all: Vec<InvocationState> = state
                                .invocations
                                .lock()
                                .unwrap()
                                .values()
                                .cloned()
                                .collect();
                            serde_json::to_value(all).map_err(|e| WireError::Internal {
                                message: e.to_string(),
                            })
                        }
                    }
                },
            )
            .expect("register invocation view");

        let state = self.clone();
        registry
            .command::<DropInput, _, _>(invocation_drop(), move |input: DropInput| {
                let state = state.clone();
                async move {
                    let mut invocations = state.invocations.lock().unwrap();
                    let Some(row) = invocations.get_mut(&input.invocation_id) else {
                        return Err(WireError::InvalidInput {
                            op: "invocation.drop".into(),
                            message: format!("no invocation `{}`", input.invocation_id),
                        });
                    };
                    row.phase = "failed".to_string();
                    Ok(Receipt {
                        atoms: vec![AtomRef {
                            domain: Domain::Event,
                            seq: state.next_seq(),
                        }],
                    })
                }
            })
            .expect("register drop");

        let state = self.clone();
        registry
            .command::<PublishInput, _, _>(trigger_publish(), move |_input: PublishInput| {
                let state = state.clone();
                async move {
                    Ok(Receipt {
                        atoms: vec![AtomRef {
                            domain: Domain::Trigger,
                            seq: state.next_seq(),
                        }],
                    })
                }
            })
            .expect("register publish");

        registry
            .command::<DownInput, _, _>(control_down(), |_input: DownInput| async move {
                Ok(Receipt { atoms: vec![] })
            })
            .expect("register down");

        registry
            .synthetic::<ControlState, _, _>(control(), || async move {
                Ok(ControlState {
                    version: "mock".to_string(),
                    nats_connected: true,
                    stream_ok: true,
                })
            })
            .expect("register control");

        registry
            .report::<fq_ops::fixtures::CostParams, fq_ops::fixtures::CostOutput, _, _>(
                cost_summary(),
                |_params| async move {
                    Ok(fq_ops::fixtures::CostOutput {
                        total_cost: 1.25,
                        total_llm_calls: 7,
                    })
                },
            )
            .expect("register cost report");

        registry
    }
}

/// A served mock edge: everything a consumer test needs to connect.
pub struct TestEdge {
    pub addr: SocketAddr,
    pub fingerprint: [u8; 32],
    pub admin_token: String,
    pub identity: EdgeIdentity,
    pub domain: MockDomain,
}

/// Provision an identity, build the seeded mock domain, and serve it
/// on an ephemeral loopback port. The serving task runs until the
/// test's runtime drops.
pub async fn spawn_edge() -> anyhow::Result<TestEdge> {
    let identity = EdgeIdentity::provision()?;
    let domain = MockDomain::seeded();
    let registry = Arc::new(domain.registry());
    let (addr, serving) = crate::server::bind("127.0.0.1:0", &identity, registry).await?;
    tokio::spawn(serving);
    Ok(TestEdge {
        addr,
        fingerprint: identity.fingerprint(),
        admin_token: identity.mint_admin_token()?,
        domain,
        identity,
    })
}
