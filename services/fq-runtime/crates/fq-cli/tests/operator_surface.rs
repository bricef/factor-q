//! The operator surface as a reviewable artifact: the daemon's real
//! registry, described and committed. Any change to what the daemon
//! promises — a new op, a schema change, a stability shift — is a
//! visible diff in this file at review time, which is the "dedicated
//! interface file" property without a second source of truth: the
//! snapshot is generated FROM the declarations, never edited.
//! Regenerate after an intentional change with
//! `UPDATE_SNAPSHOT=1 cargo test -p fq-cli --test operator_surface`.

use std::sync::Arc;
use std::time::Duration;

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
                .store(worker_store)
                .worker_id(fq_runtime::worker::WorkerId::new("snapshot-worker").unwrap())
                .build(),
        ),
        fq_runtime::Harness::new(),
    ));
    let registry = fq_cli::operator_registry(
        views,
        fq_runtime::watermark::Horizon::new(vec![watermark]),
        Duration::from_millis(1),
        fq_cli::OperatorDeps {
            bus,
            projection: projection_store,
            control_plane: control_plane_store,
            runner,
            // The Agent view's source. Empty here: the snapshot
            // describes the surface's shape, and a registry's contents
            // are data, not declaration.
            agents: fq_runtime::shared_registry(fq_runtime::AgentRegistry::new()),
        },
    )
    .expect("assemble the operator registry");
    let actual = serde_json::to_string_pretty(&registry.describe_value().expect("describe"))
        .expect("serialise")
        + "\n";

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots/operator_surface.json");
    if std::env::var_os("UPDATE_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing snapshot {path:?} — run `UPDATE_SNAPSHOT=1 cargo test -p fq-cli \
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
