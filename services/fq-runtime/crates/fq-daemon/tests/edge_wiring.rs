//! The daemon hosts the authenticated operator edge (plan Phase 2,
//! PR 2): with `[edge]` enabled, the first run provisions an identity
//! under the state dir and prints the admin token + certificate
//! fingerprint exactly once; a client pinning that fingerprint and
//! presenting that token reaches `List(Operation)` end-to-end. A
//! restart reuses the persisted identity — same fingerprint, the old
//! token still works, and nothing secret is printed again.
//!
//! And the identity survives its own relocation: a deployment whose
//! identity still sits under the cache dir (the pre-#362 home) adopts
//! it on upgrade rather than minting a new root.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId};
use serde_json::json;

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-wiring-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(
        dir.join("fq.toml"),
        "[edge]\nenabled = true\nbind = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    dir
}

fn spawn_daemon(
    scratch: &std::path::Path,
    nats_url: &str,
    log: &std::path::Path,
) -> std::process::Child {
    spawn_daemon_with_state(scratch, &scratch.join("state"), nats_url, log)
}

/// `state_dir` is explicit so the migration test can stand a daemon up
/// in the pre-#362 layout (state == cache) and then move it.
fn spawn_daemon_with_state(
    scratch: &std::path::Path,
    state_dir: &std::path::Path,
    nats_url: &str,
    log: &std::path::Path,
) -> std::process::Child {
    let file = std::fs::File::create(log).expect("create daemon log");
    let file_err = file.try_clone().expect("clone log handle");
    Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", state_dir)
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("spawn fqd")
}

async fn wait_for_ready(child: &mut std::process::Child, log: &std::path::Path) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains("Runtime ready") {
            return text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fqd never reached 'Runtime ready' within 30s\n--- log ---\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn terminate(mut child: std::process::Child) {
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) failed");
    let status = child.wait().expect("wait for fqd");
    assert!(
        status.success(),
        "expected clean exit on SIGTERM, got {status:?}"
    );
}

fn line_after<'a>(log: &'a str, needle: &str) -> &'a str {
    let mut lines = log.lines();
    lines
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("log lacks {needle:?}\n--- log ---\n{log}"));
    lines.next().expect("line after marker").trim()
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    assert_eq!(hex.len(), 64, "fingerprint must be 32 hex bytes: {hex:?}");
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

async fn describe_via(addr: &str, fingerprint: [u8; 32], token: &str) -> serde_json::Value {
    let client = fq_edge::EdgeClient::connect(addr, fingerprint, token)
        .await
        .expect("connect to the edge");
    client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: OpId::List(Domain::Operation),
                version: 1,
                input: json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("List(Operation)")
        .output
}

#[tokio::test]
async fn first_run_provisions_and_restart_reuses_the_identity() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();

    // First run: identity provisioned, admin token printed once.
    let log1 = scratch.join("daemon-1.log");
    let mut child = spawn_daemon(&scratch, server.url(), &log1);
    let text1 = wait_for_ready(&mut child, &log1).await;

    let fingerprint = parse_fingerprint(suffix_of(
        &text1,
        "edge: certificate fingerprint (clients pin this): ",
    ));
    let token = line_after(&text1, "edge: admin token").to_string();
    let addr1 = suffix_of(&text1, "- edge is listening on ").to_string();

    // The surface describes itself to an authenticated caller — the
    // real catalogue, carrying the Invocation view (Phase 3b).
    let described = describe_via(&addr1, fingerprint, &token).await;
    let entries = described.as_array().expect("describe is a list");
    assert!(
        entries.iter().any(|e| e.get("view").is_some()),
        "describe carries the Invocation view: {described}"
    );
    terminate(child);

    // Restart on the same state dir: the identity is reused, the old
    // token still opens the door, and no secret is printed again.
    let log2 = scratch.join("daemon-2.log");
    let mut child = spawn_daemon(&scratch, server.url(), &log2);
    let text2 = wait_for_ready(&mut child, &log2).await;
    assert!(
        !text2.contains("admin token") && !text2.contains("first run"),
        "restart must not re-provision or re-print secrets\n--- log ---\n{text2}"
    );

    let addr2 = suffix_of(&text2, "- edge is listening on ").to_string();
    let described = describe_via(&addr2, fingerprint, &token).await;
    assert!(
        described.as_array().is_some_and(|e| !e.is_empty()),
        "restarted edge honours the original token: {described}"
    );
    terminate(child);

    let _ = std::fs::remove_dir_all(&scratch);
}

/// #362 regression, the one that matters: a deployment already paired
/// against an identity under the *cache* directory upgrades to a
/// binary that keeps the identity under the *state* directory. The old
/// identity must be adopted, not re-minted — the fingerprint pinned by
/// every client and the tokens already issued have to keep working. A
/// naive move would deliver exactly the orphaning the issue exists to
/// prevent.
#[tokio::test]
async fn a_legacy_cache_identity_is_adopted_and_keeps_its_fingerprint() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();
    let cache = scratch.join("cache");
    let state = scratch.join("state");

    // The pre-#362 world: state dir == cache dir, so the identity is
    // provisioned at <cache>/edge exactly as the old binary did.
    let log1 = scratch.join("legacy.log");
    let mut child = spawn_daemon_with_state(&scratch, &cache, server.url(), &log1);
    let text1 = wait_for_ready(&mut child, &log1).await;
    let fingerprint = parse_fingerprint(suffix_of(
        &text1,
        "edge: certificate fingerprint (clients pin this): ",
    ));
    let token = line_after(&text1, "edge: admin token").to_string();
    terminate(child);
    assert!(
        cache.join("edge").join("cert.der").exists(),
        "the legacy layout puts the identity under the cache dir"
    );

    // The upgrade: same cache, state now resolves elsewhere.
    let log2 = scratch.join("adopted.log");
    let mut child = spawn_daemon_with_state(&scratch, &state, server.url(), &log2);
    let text2 = wait_for_ready(&mut child, &log2).await;
    assert!(
        text2.contains("edge: identity adopted from"),
        "the migration must announce itself\n--- log ---\n{text2}"
    );
    assert!(
        !text2.contains("first run") && !text2.contains("admin token"),
        "adoption is not a fresh mint\n--- log ---\n{text2}"
    );
    assert!(
        state.join("edge").join("cert.der").exists(),
        "the identity is now authoritative under the state dir"
    );
    assert!(
        cache.join("edge").join("cert.der").exists(),
        "the legacy copy is left in place, so a rollback still works"
    );

    // The proof: a client pinning the ORIGINAL fingerprint, presenting
    // the ORIGINAL token, still reaches the edge.
    let addr = suffix_of(&text2, "- edge is listening on ").to_string();
    let described = describe_via(&addr, fingerprint, &token).await;
    assert!(
        described.as_array().is_some_and(|e| !e.is_empty()),
        "the pin and the token survived the move: {described}"
    );
    terminate(child);

    // The hardening #360 added survives the copy.
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&state.join("edge")), 0o700);
    for name in ["key.der", "root.key"] {
        assert_eq!(mode(&state.join("edge").join(name)), 0o600, "{name}");
    }

    let _ = std::fs::remove_dir_all(&scratch);
}
