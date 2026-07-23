//! The client side of the edge, driven through the real `fq` binary
//! against a live daemon (plan Phase 2, 2b): `fq connect` pairs via
//! TOFU (auto-accepting with a notice when stdin is not a terminal),
//! stores credentials user-side at 0600; `fq ops list` speaks the
//! authenticated surface; a tampered pin is refused with operator
//! guidance; `--fingerprint` re-pins explicitly; and
//! `fq token attenuate` narrows offline to a token the daemon still
//! honours.

#![cfg(unix)]

use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-cli-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::create_dir_all(dir.join("xdg")).unwrap();
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

/// Run the `fq` binary with stdin piped (non-interactive) and the
/// scratch dir's isolated XDG config home.
fn fq(scratch: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(args)
        .env("FQ_CONFIG", scratch.join("fq.toml"))
        .env("XDG_CONFIG_HOME", scratch.join("xdg"))
        .stdin(Stdio::piped())
        .output()
        .expect("run fq")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn line_after<'a>(log: &'a str, needle: &str) -> &'a str {
    let mut lines = log.lines();
    lines
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("log lacks {needle:?}\n--- log ---\n{log}"));
    lines.next().expect("line after marker").trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    assert_eq!(hex.len(), 64, "fingerprint must be 32 hex bytes: {hex:?}");
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

#[tokio::test]
async fn the_cli_pairs_lists_repins_and_attenuates() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();

    // A live daemon with the edge up; harvest its printed credentials.
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Some(status) = daemon.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if text.contains("Runtime ready") {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fqd never reached 'Runtime ready'\n--- log ---\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let fingerprint_hex =
        suffix_of(&text, "edge: certificate fingerprint (clients pin this): ").to_string();
    let admin_token = line_after(&text, "edge: admin token").to_string();
    let addr = suffix_of(&text, "- edge is listening on ").to_string();

    // --- fq connect: TOFU, non-interactive → auto-accept + notice.
    let out = fq(&scratch, &["connect", &addr, "--token", &admin_token]);
    let err = stderr_of(&out);
    assert!(out.status.success(), "fq connect failed:\n{err}");
    assert!(
        err.contains(&fingerprint_hex) && err.contains("non-interactive: pinning automatically"),
        "TOFU must show the fingerprint and say it auto-pinned:\n{err}"
    );
    assert!(
        err.contains("Compare it with the fingerprint the daemon printed"),
        "TOFU must tell the operator what to compare against:\n{err}"
    );

    // Credentials landed user-side, owner-only.
    let creds_path = scratch.join("xdg/factor-q/connections.toml");
    let creds = std::fs::read_to_string(&creds_path).expect("connections.toml written");
    assert!(
        creds.contains(&fingerprint_hex),
        "stored entry pins the daemon's fingerprint:\n{creds}"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&creds_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials must be owner-only, got {mode:o}");
    }

    // --- fq ops list: the authenticated surface, empty until Phase 3.
    let out = fq(&scratch, &["ops", "list", "--addr", &addr]);
    assert!(
        out.status.success(),
        "fq ops list failed:\n{}",
        stderr_of(&out)
    );
    assert!(
        stdout_of(&out).contains("no operations registered"),
        "empty registry renders honestly:\n{}",
        stdout_of(&out)
    );
    let out = fq(&scratch, &["ops", "list", "--addr", &addr, "--json"]);
    assert_eq!(stdout_of(&out).trim(), "[]", "empty registry as JSON");

    // --- a tampered pin is refused with guidance, never re-pinned
    // silently.
    let tampered = creds.replace(
        &fingerprint_hex,
        &format!(
            "{}{}",
            if fingerprint_hex.starts_with('0') {
                "1"
            } else {
                "0"
            },
            &fingerprint_hex[1..]
        ),
    );
    std::fs::write(&creds_path, tampered).unwrap();
    let out = fq(&scratch, &["ops", "list", "--addr", &addr]);
    assert!(!out.status.success(), "tampered pin must refuse");
    assert!(
        stderr_of(&out).contains("does not match the pinned fingerprint"),
        "mismatch needs its distinct, actionable error:\n{}",
        stderr_of(&out)
    );

    // --- explicit re-pin with --fingerprint recovers.
    let out = fq(
        &scratch,
        &[
            "connect",
            &addr,
            "--token",
            &admin_token,
            "--fingerprint",
            &fingerprint_hex,
        ],
    );
    assert!(
        out.status.success(),
        "explicit re-pin failed:\n{}",
        stderr_of(&out)
    );
    let out = fq(&scratch, &["ops", "list", "--addr", &addr]);
    assert!(
        out.status.success(),
        "list after re-pin:\n{}",
        stderr_of(&out)
    );

    // --- fq token attenuate: offline narrowing the daemon honours.
    let out = fq(
        &scratch,
        &["token", "attenuate", "--grant", "read:*", "--addr", &addr],
    );
    assert!(
        out.status.success(),
        "attenuate failed:\n{}",
        stderr_of(&out)
    );
    let narrowed = stdout_of(&out).trim().to_string();
    assert!(!narrowed.is_empty() && narrowed != admin_token);
    let client =
        fq_edge::EdgeClient::connect(&addr, parse_fingerprint(&fingerprint_hex), &narrowed)
            .await
            .expect("the attenuated token still opens the door");
    let described = client
        .rpc
        .invoke(
            tarpc::context::current(),
            fq_edge::InvokeRequest {
                op: fq_ops::OpId::List(fq_ops::Domain::Operation),
                version: 1,
                input: serde_json::json!({}),
                min_seq: None,
            },
        )
        .await
        .expect("rpc")
        .expect("read:* covers operation.list");
    assert_eq!(described.output, serde_json::json!([]));

    // --- malformed grant refused before any datalog is built.
    let out = fq(
        &scratch,
        &["token", "attenuate", "--grant", "read", "--addr", &addr],
    );
    assert!(
        !out.status.success(),
        "grant without a colon must be refused"
    );
    assert!(
        stderr_of(&out).contains("verb:domain"),
        "grant-shape error names the expected form:\n{}",
        stderr_of(&out)
    );

    let rc = unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) failed");
    let status = daemon.wait().expect("wait for fqd");
    assert!(status.success(), "clean daemon exit, got {status:?}");
    let _ = std::fs::remove_dir_all(&scratch);
}
