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

/// The escalation ladder, which the one-shot could not express:
/// `--now` on a daemon already draining raises the mode, a repeated
/// plain `down` changes nothing, and nothing can lower it again.
#[test]
fn the_stop_mode_escalates_and_never_relaxes() {
    let signal = DownSignal::new();
    let rx = signal.subscribe();
    assert_eq!(*rx.borrow(), None, "no stop has been asked for yet");

    assert!(signal.request(DownMode::Drain), "the first ask is a change");
    assert!(
        !signal.request(DownMode::Drain),
        "a repeated plain `down` on a daemon already stopping changes nothing"
    );
    assert_eq!(*rx.borrow(), Some(DownMode::Drain));

    assert!(
        signal.request(DownMode::Now),
        "`--now` against a draining daemon must escalate, not be swallowed"
    );
    assert_eq!(*rx.borrow(), Some(DownMode::Now));

    assert!(
        !signal.request(DownMode::Drain),
        "a plain `down` arriving after a `--now` must not put the drain back"
    );
    assert_eq!(*rx.borrow(), Some(DownMode::Now));
}

/// A daemon whose edge registry has been dropped has nobody left to
/// ask it to stop. The absence of an operator is not an instruction
/// from one: the wait must not resolve.
#[tokio::test]
async fn a_dropped_sender_is_not_a_stop_request() {
    let signal = DownSignal::new();
    let mut rx = signal.subscribe();
    drop(signal);

    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        wait_for_down(&mut rx),
    )
    .await;
    assert!(
        waited.is_err(),
        "a dropped stop switch reported a stop nobody asked for"
    );
}

/// The drain's escalation wait fires on `Now` and only on `Now` — a
/// plain `down` is the drain it is already running.
#[tokio::test]
async fn the_escalation_wait_ignores_a_plain_down() {
    let signal = DownSignal::new();
    let mut rx = signal.subscribe();
    signal.request(DownMode::Drain);

    let ignored = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        wait_for_down_now(&mut rx),
    )
    .await;
    assert!(ignored.is_err(), "a plain `down` must not escalate itself");

    signal.request(DownMode::Now);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_down_now(&mut rx),
    )
    .await
    .expect("`--now` must wake the drain's escalation wait");
}
