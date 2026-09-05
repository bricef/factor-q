//! The operator surface as a reviewable artifact: the daemon's real
//! registry, described and committed. Any change to what the daemon
//! promises — a new op, a schema change, a stability shift — is a
//! visible diff in this file at review time, which is the "dedicated
//! interface file" property without a second source of truth: the
//! snapshot is generated FROM the declarations, never edited.
//! Regenerate after an intentional change with
//! `UPDATE_SNAPSHOT=1 cargo test -p fq-daemon --test operator_surface`.

use std::sync::Arc;
use std::time::Duration;

/// A model that is declared but never asked. `invocation.resume`'s
/// handle carries the LLM it would re-drive a crashed invocation with,
/// and this test only reads the shape of the surface that handle is
/// registered on — so the honest stub is one that panics if the
/// snapshot ever starts running an invocation.
struct UnusedLlm;

#[async_trait::async_trait]
impl fq_runtime::llm::LlmClient for UnusedLlm {
    async fn chat(
        &self,
        _request: fq_runtime::llm::ChatRequest,
    ) -> Result<fq_runtime::llm::ChatResponse, fq_runtime::llm::LlmError> {
        panic!("the surface snapshot describes declarations; it never calls a model")
    }
}

#[tokio::test]
async fn operator_surface_matches_the_committed_snapshot() {
    let server = fq_test_support::NatsServer::start();
    let scratch = tempfile::tempdir().unwrap();
    let paths = fq_runtime::db::RuntimeDbPaths::under(scratch.path());
    let projection_store = std::sync::Arc::new(
        fq_runtime::control_plane::projection::store::ProjectionStore::open(&paths.projection)
            .await
            .expect("init projection store"),
    );
    let control_plane_store = std::sync::Arc::new(
        fq_runtime::control_plane::store::ControlPlaneStore::open(&paths.control_plane)
            .await
            .expect("init control-plane store"),
    );
    let worker_store = Arc::new(
        fq_runtime::worker::store::WorkerStore::open(&paths.worker)
            .await
            .expect("init worker store"),
    );
    let views = Arc::new(
        fq_runtime::views::Views::open(&paths)
            .await
            .expect("open views"),
    );
    let (_watermark_tx, watermark) = fq_runtime::watermark::channel();
    let bus = fq_runtime::EventBus::connect(server.url())
        .await
        .expect("connect");
    // A real runner, driving nothing: `invocation.drop` holds one as
    // its liveness authority, and the snapshot describes the surface
    // that daemon assembles — so it is assembled the same way here.
    let runner = Arc::new(fq_runtime::ReducerRunner::new(
        Arc::new(
            fq_runtime::ReducerContext::builder()
                .tools(Arc::new(fq_runtime::ToolRegistry::new()))
                .build(),
        ),
        Arc::new(
            fq_runtime::RunnerConfig::builder()
                .bus(bus.clone())
                .pricing(Arc::new(fq_runtime::PricingTable::empty()))
                .store(worker_store.clone())
                .worker_id(fq_runtime::worker::WorkerId::new("snapshot-worker").unwrap())
                .build(),
        ),
        fq_runtime::Harness::new(),
    ));
    // The agent registry the daemon shares between the Agent view and
    // the resume path — one handle, as `fqd` wires it.
    let agents = fq_runtime::shared_registry(fq_runtime::AgentRegistry::new());
    // `invocation.resume` is a command on the edge now, so its handle
    // is assembled here rather than handed to a NATS listener.
    let resume = Arc::new(fq_daemon::ResumeControl::new(
        bus.clone(),
        worker_store,
        control_plane_store.clone(),
        runner.clone(),
        agents.clone(),
        Arc::new(UnusedLlm),
    ));
    let registry = fq_daemon::operator_registry(
        views,
        fq_runtime::watermark::Horizon::new(vec![watermark]),
        Duration::from_millis(1),
        fq_daemon::OperatorDeps {
            facts: fq_daemon::DaemonFacts {
                db_paths: Arc::new(paths.clone()),
                legacy_events_db: Arc::new(fq_runtime::db::legacy_db_path(scratch.path())),
                drain_deadline_ms: 180_000,
            },
            bus,
            projection: projection_store,
            control_plane: control_plane_store,
            runner: runner.clone(),
            resume,
            // The Agent view's source. Empty here: the snapshot
            // describes the surface's shape, and a registry's contents
            // are data, not declaration.
            agents,
            // What the machinery verbs command. Wired, never thrown:
            // the snapshot describes declarations, and a stop switch
            // nobody touches declares `control.down` just as well as
            // one that would stop a daemon.
            machinery: fq_daemon::MachineryDeps {
                agents: fq_runtime::shared_registry(fq_runtime::AgentRegistry::new()),
                agents_dir: scratch.path().join("agents"),
                default_model: None,
                worker: runner.clone(),
                down: fq_daemon::DownSignal::new(),
            },
        },
    )
    .expect("assemble the operator registry");
    // Canonical rather than `to_string_pretty`: `serde_json::Map` is a
    // `BTreeMap` or an `IndexMap` depending on whether something in the
    // build graph enables `preserve_order`, so raw serialisation makes the
    // expected bytes depend on which packages are compiled together
    // (#437). Sorting keys here makes the snapshot a function of the data.
    let actual = fq_test_support::canonical_json(&registry.describe_value().expect("describe"));

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/operator_surface.json");
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing snapshot {path:?} — run `UPDATE_SNAPSHOT=1 cargo test -p fq-daemon \
             --test operator_surface` and commit the result"
        )
    });
    assert_eq!(
        actual, expected,
        "the operator surface drifted from its committed snapshot. If intentional, \
         review the diff against P10's additive-change rules, then UPDATE_SNAPSHOT=1 \
         and commit."
    );
}
