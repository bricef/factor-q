//! The drain wait, with a stub holding the drain open.
//!
//! A real daemon's drain ends in milliseconds when nothing is in
//! flight, and what needs proving is what happens while it is *still*
//! waiting — so the dispatcher handle here is a task that never
//! finishes. That is the whole fixture: with it, the deadline, the
//! second signal and `fq down --now` each become a deterministic
//! outcome rather than a race against a drain that has already
//! finished.
//!
//! Holding a real subprocess daemon's drain open would take a stalled
//! LLM provider (review B1's condition), which is a different issue's
//! machinery; `tests/daemon_shutdown.rs` covers the end-to-end promise
//! that a second signal never costs the clean teardown.
//!
//! **Why the lock.** `libc::raise(SIGTERM)` is process-wide and tokio
//! broadcasts a delivered signal to every registration in the process,
//! so a test that raises one would escalate any other drain wait
//! running concurrently in the same binary. The tests that touch
//! signals take a shared lock and the raising one runs last within it.

use std::sync::OnceLock;

use tokio::sync::Mutex;

use super::*;
use crate::control_commands::{DownMode, DownSignal};

/// Serialises everything that installs a signal handler or raises a
/// signal — see the module note.
fn signal_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A dispatcher that never stops: the stub that holds the drain open.
fn a_drain_that_never_finishes() -> Option<tokio::task::JoinHandle<Result<(), String>>> {
    Some(tokio::spawn(std::future::pending()))
}

/// What the shutdown select hands over when its OWN dispatcher arm won
/// — the handle is already consumed.
fn an_already_joined_dispatcher() -> Option<tokio::task::JoinHandle<Result<(), String>>> {
    None
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(3_600)
}

/// The ordinary case: the work suspends inside the deadline.
#[tokio::test]
async fn a_drain_that_suspends_reports_suspended() {
    let _guard = signal_lock().lock().await;
    let mut signals = ShutdownSignals::install();
    let mut down = DownSignal::new().subscribe();
    let dispatcher: Option<tokio::task::JoinHandle<Result<(), String>>> =
        Some(tokio::spawn(async { Ok(()) }));
    let resume = vec![tokio::spawn(async {}), tokio::spawn(async {})];

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_drain(far_future(), dispatcher, resume, &mut signals, &mut down),
    )
    .await
    .expect("a drain with nothing to wait for must not hang");
    assert_eq!(outcome, DrainOutcome::Suspended);
}

/// Past the deadline the stragglers are abandoned — the next start's
/// recovery resumes them — and the wait must end rather than become
/// the wedge it is bounded to prevent.
#[tokio::test]
async fn a_drain_that_will_not_suspend_ends_at_its_deadline() {
    let _guard = signal_lock().lock().await;
    let mut signals = ShutdownSignals::install();
    let mut down = DownSignal::new().subscribe();

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_drain(
            Instant::now() + Duration::from_millis(200),
            a_drain_that_never_finishes(),
            vec![tokio::spawn(std::future::pending())],
            &mut signals,
            &mut down,
        ),
    )
    .await
    .expect("the drain deadline must bound the wait");
    assert_eq!(outcome, DrainOutcome::DeadlineElapsed);
}

/// `fq down --now` against a daemon already draining. The one-shot it
/// replaced made this a no-op, which is precisely when an operator
/// reaches for it (#509).
#[tokio::test]
async fn an_immediate_down_escalates_a_running_drain() {
    let _guard = signal_lock().lock().await;
    let mut signals = ShutdownSignals::install();
    let signal = DownSignal::new();
    let mut down = signal.subscribe();
    signal.request(DownMode::Drain);

    let escalate = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.request(DownMode::Now);
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_drain(
            far_future(),
            a_drain_that_never_finishes(),
            Vec::new(),
            &mut signals,
            &mut down,
        ),
    )
    .await
    .expect("`fq down --now` must cut a running drain short");
    escalate.await.unwrap();
    assert_eq!(outcome, DrainOutcome::Escalated { reason: "down_now" });
}

/// The #509 promise, made true: the drain holds the signal streams, so
/// the operator's second SIGTERM escalates it. It returns
/// `Escalated` rather than aborting the process, which is what keeps
/// the deregistration and the `system.shutdown` publish — the two
/// things `SIG_DFL` would have skipped.
///
/// Raises a real SIGTERM at itself; the handler `ShutdownSignals`
/// installed is what stops that killing the test binary.
#[tokio::test]
async fn a_second_sigterm_escalates_a_running_drain() {
    let _guard = signal_lock().lock().await;
    let mut signals = ShutdownSignals::install();
    let mut down = DownSignal::new().subscribe();

    // The first SIGTERM — the one that started the drain — is read and
    // consumed exactly as `run_hosted` reads it.
    assert_eq!(raise_sigterm_and_read(&mut signals).await, "sigterm");

    // The operator, watching a drain that will not finish.
    raise_sigterm();

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_drain(
            far_future(),
            a_drain_that_never_finishes(),
            vec![tokio::spawn(std::future::pending())],
            &mut signals,
            &mut down,
        ),
    )
    .await
    .expect("a second SIGTERM must end the drain wait, not be absorbed");
    assert_eq!(outcome, DrainOutcome::Escalated { reason: "sigterm" });
}

fn raise_sigterm() {
    assert_eq!(
        unsafe { libc::raise(libc::SIGTERM) },
        0,
        "raise(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
}

async fn raise_sigterm_and_read(signals: &mut ShutdownSignals) -> &'static str {
    raise_sigterm();
    tokio::time::timeout(Duration::from_secs(5), signals.next())
        .await
        .expect("the installed handler never saw the signal")
}

/// Finding 1: the shutdown select's own dispatcher arm consumes the
/// handle, and a `JoinHandle` polled after completion panics. That
/// panic would land in the daemon's main task, skipping the MCP
/// shutdown, the worker deregistration and the `system.shutdown`
/// publish — the whole teardown. Both joins must accept a handle that
/// is already gone.
#[tokio::test]
async fn a_dispatcher_the_select_already_joined_is_not_joined_again() {
    let _guard = signal_lock().lock().await;
    let mut signals = ShutdownSignals::install();
    let mut down = DownSignal::new().subscribe();

    // The drain path: nothing left to wait for but the resume tasks.
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_drain(
            far_future(),
            an_already_joined_dispatcher(),
            vec![tokio::spawn(async {})],
            &mut signals,
            &mut down,
        ),
    )
    .await
    .expect("re-polling a consumed dispatcher handle panicked the drain wait");
    assert_eq!(outcome, DrainOutcome::Suspended);

    // The non-drain path: a fast stop, or the task-failure exit that
    // reaches this join precisely because the dispatcher arm won.
    tokio::time::timeout(
        Duration::from_secs(5),
        join_dispatcher(
            an_already_joined_dispatcher(),
            Instant::now() + Duration::from_secs(1),
        ),
    )
    .await
    .expect("re-polling a consumed dispatcher handle panicked the teardown");
}

/// The escalated reason names both halves, so the event log can tell a
/// deploy's SIGTERM cut short by an operator's `fq down --now` from a
/// Ctrl-C pressed twice.
#[test]
fn an_escalated_reason_names_the_stop_and_the_escalator() {
    assert_eq!(
        escalated_reason("sigterm", "sigterm"),
        "sigterm_escalated_by_sigterm"
    );
    assert_eq!(
        escalated_reason("sigterm", "down_now"),
        "sigterm_escalated_by_down_now"
    );
    assert_eq!(
        escalated_reason("sigterm", "ctrl_c"),
        "sigterm_escalated_by_ctrl_c"
    );
    assert_eq!(
        escalated_reason("down", "sigterm"),
        "down_escalated_by_sigterm"
    );
    assert_eq!(
        escalated_reason("down", "ctrl_c"),
        "down_escalated_by_ctrl_c"
    );
    assert_eq!(
        escalated_reason("down", "down_now"),
        "down_escalated_by_down_now"
    );
}
