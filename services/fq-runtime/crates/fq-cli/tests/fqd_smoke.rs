//! `fqd` is the daemon and nothing else — but it must be the *same*
//! daemon `fq run` starts: shared code, shared behaviour. This smoke
//! proves the new binary reaches steady state and drains cleanly on
//! SIGTERM, exactly like `daemon_shutdown.rs` proves for `fq run`.

#![cfg(unix)]

use std::io::ErrorKind;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fqd-smoke-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    // The edge is on by default; an ephemeral port keeps parallel
    // daemon-spawning tests from fighting over the fixed default bind.
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}

#[test]
fn fqd_reaches_steady_state_and_drains_on_sigterm() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{log}");
        }
        if std::fs::read_to_string(&log_path)
            .unwrap_or_default()
            .contains("Runtime ready")
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "fqd never reached 'Runtime ready' within 30s");

    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) failed");

    let status = wait_with_timeout(&mut child, Duration::from_secs(15))
        .expect("fqd did not exit within 15s of SIGTERM");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        status.success(),
        "expected clean exit(0) on SIGTERM, got {status:?}\n--- log ---\n{log}"
    );
    assert!(
        log.contains("Received SIGTERM, draining..."),
        "fqd did not take the drain path — is it running the shared daemon code?\n--- log ---\n{log}"
    );
    assert!(
        log.contains("edge is listening on"),
        "the edge is on by default and must reach steady state with the daemon\n--- log ---\n{log}"
    );
}
