//! The broker credential never leaves `Config` (#540). A daemon started
//! against a token-authenticated broker through `[nats] token_env` must
//! not print the token anywhere an operator or a `read:event` holder can
//! see it: the banner (stdout), the tracing log (stderr, here at trace
//! level for the crates that touch the connection), or the
//! `system.startup` payload that `fq events get` serves whole.
//!
//! The guarantee is structural, not a scrub: the URL the daemon prints is
//! the URL it validated (userinfo refused), and the token only ever
//! reaches the connect options. This test is the end-to-end proof that
//! the construction holds — and its positive assertions (the clean URL
//! *is* in the banner and in the payload, the daemon *did* authenticate)
//! keep it from passing vacuously.

#![cfg(unix)]

use std::io::ErrorKind;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "fqd-token-hygiene-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
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

/// The variable the scratch config names. Deliberately not
/// `FQ_NATS_TOKEN`: the daemon must read the variable the config names,
/// not a conventional one it happens to know.
const TOKEN_ENV: &str = "FQ_TEST_BROKER_TOKEN";

#[test]
fn token_reaches_the_broker_but_never_the_banner_log_or_startup_event() {
    // Unique and unlike anything else the daemon prints, so a substring
    // search is a real test and not a coincidence either way.
    let token = format!("leakcheck-{}", uuid::Uuid::now_v7().simple());
    let server = fq_test_support::NatsServer::start_with_token(&token);
    let nats_url = server.url().to_string();
    assert!(
        !nats_url.contains(&token),
        "test setup: URL carries the token"
    );

    let scratch = unique_scratch();
    // The edge is on by default; an ephemeral port keeps the parallel
    // daemon-spawning tests from fighting over the fixed default bind.
    std::fs::write(
        scratch.join("fqd.toml"),
        format!("[edge]\nbind = \"127.0.0.1:0\"\n\n[nats]\ntoken_env = \"{TOKEN_ENV}\"\n"),
    )
    .unwrap();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fqd.toml"))
        .env("FQ_NATS_URL", &nats_url)
        .env(TOKEN_ENV, &token)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        // Trace for everything that handles the connection, so a
        // debug/trace line that echoed the connect options would fail
        // this test rather than wait for an operator to set RUST_LOG.
        .env(
            "RUST_LOG",
            "info,fqd=trace,fq_daemon=trace,fq_runtime=trace,async_nats=trace",
        )
        // JSON lines: the text formatter wraps field names in ANSI
        // escapes, which would make the positive check below a check on
        // the colour scheme. The token search is format-independent.
        .env("FQ_LOG_FORMAT", "json")
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
    assert!(
        ready,
        "fqd never reached 'Runtime ready' within 30s — did the token reach the broker?\n--- log ---\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_default()
    );

    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) failed");
    let status = wait_with_timeout(&mut child, Duration::from_secs(15))
        .expect("fqd did not exit within 15s of SIGTERM");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        status.success(),
        "expected clean exit, got {status:?}\n--- log ---\n{log}"
    );

    // 1. The banner and every log line: the clean URL is there, the
    //    token is not.
    assert!(
        log.contains(&format!("NATS:             {nats_url}")),
        "the banner should print the credential-free URL\n--- log ---\n{log}"
    );
    assert!(
        log.contains("connecting to NATS") && log.contains("\"token_auth\":true"),
        "the bus should have logged a token-authenticated connect\n--- log ---\n{log}"
    );
    assert!(
        !log.contains(&token),
        "the broker token leaked into stdout or the log\n--- log ---\n{log}"
    );

    // 2. The `system.startup` payload, read back from the stream exactly
    //    as `event.get` would serve it. Reading it needs the token too —
    //    which is also the proof the daemon authenticated with it.
    let raw = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            let client = async_nats::ConnectOptions::with_token(token.clone())
                .connect(&nats_url)
                .await
                .expect("connect to the token broker for verification");
            let stream = async_nats::jetstream::new(client)
                .get_stream(fq_runtime::bus::STREAM_NAME)
                .await
                .expect("the daemon ensured the event stream");
            let message = stream
                .get_last_raw_message_by_subject(fq_runtime::events::subjects::SYSTEM_STARTUP)
                .await
                .expect("the daemon published system.startup");
            String::from_utf8(message.payload.to_vec()).expect("utf-8 event")
        });
    let _ = std::fs::remove_dir_all(&scratch);

    let event: serde_json::Value = serde_json::from_str(&raw).expect("system.startup is JSON");
    assert_eq!(
        // `{"envelope": …, "payload": {"event_type": …, "payload": {…}}}`
        event
            .pointer("/payload/payload/nats_url")
            .and_then(|v| v.as_str()),
        Some(nats_url.as_str()),
        "the startup payload should carry the credential-free URL: {raw}"
    );
    assert!(
        !raw.contains(&token),
        "the broker token leaked into the system.startup event: {raw}"
    );
}
