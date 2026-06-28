//! End-to-end test of the `fq-cas` binary: put a file, then read it back by
//! its content id, and check `has` / `size`.
#![cfg(feature = "cli")]

use std::process::Command;

fn fq_cas() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fq-cas"))
}

#[test]
fn put_then_get_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let input = dir.path().join("input.txt");
    let payload = b"hello content-addressed world";
    std::fs::write(&input, payload).unwrap();

    // put -> prints the content id
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("put")
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "put failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(cid.len(), 64, "cid should be 64 hex chars");

    // get -> the original bytes
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("get")
        .arg(&cid)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, payload);

    // has -> true, exit success
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("has")
        .arg(&cid)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");

    // size -> payload length
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("size")
        .arg(&cid)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        payload.len().to_string()
    );

    // has for an absent id -> exit failure
    let absent = "0".repeat(64);
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("has")
        .arg(absent)
        .output()
        .unwrap();
    assert!(!out.status.success());

    // metrics -> human-readable, reports the one stored object
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("metrics")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("objects:"), "metrics output: {text}");
    assert!(text.contains("dedup ratio:"), "metrics output: {text}");

    // metrics --json -> machine-readable, objects == 1
    let out = fq_cas()
        .args(["--root".as_ref(), root.as_os_str()])
        .arg("metrics")
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"objects\": 1"));
}
