use super::*;
use fq_runtime::agent::AgentId;
use tempfile::tempdir;

fn write_agent(dir: &Path, name: &str) {
    std::fs::write(
        dir.join(format!("{name}.md")),
        format!("---\nname: {name}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."),
    )
    .unwrap();
}

/// A reload re-reads the agents directory and swaps the shared
/// handle in place: the same `SharedRegistry` the dispatcher
/// holds now points at the freshly-loaded registry, so the next
/// trigger sees the new agent set.
#[tokio::test]
async fn reload_agents_swaps_in_new_definitions() {
    let dir = tempdir().unwrap();
    write_agent(dir.path(), "first");

    let initial = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
    assert_eq!(initial.len(), 1);
    let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

    // Add a second agent on disk, then reload.
    write_agent(dir.path(), "second");
    reload_agents(&shared, dir.path(), None).await.unwrap();

    let after = shared.read().await.clone();
    assert_eq!(after.len(), 2, "reload should pick up the new agent");
    assert!(after.get(&AgentId::new("second").unwrap()).is_some());
}

/// A reload against a directory that has gone missing keeps the
/// current registry rather than blanking it — a bad edit can't
/// knock out a running daemon — and now *says so*, which is the
/// whole gain of moving off the fire-and-forget publish: the
/// operator learns their reload did not happen.
#[tokio::test]
async fn reload_agents_keeps_current_registry_on_load_error() {
    let dir = tempdir().unwrap();
    write_agent(dir.path(), "keep");
    let initial = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
    assert_eq!(initial.len(), 1);
    let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

    // Point the reload at a directory that does not exist.
    let missing = dir.path().join("does-not-exist");
    let err = reload_agents(&shared, &missing, None)
        .await
        .expect_err("a directory that cannot be read is a failed reload");
    assert!(
        err.contains("keeping the definitions already loaded"),
        "the refusal must say the daemon is unchanged; got: {err}"
    );

    let after = shared.read().await.clone();
    assert_eq!(after.len(), 1, "failed reload must keep the old registry");
    assert!(after.get(&AgentId::new("keep").unwrap()).is_some());
}
