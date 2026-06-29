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

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    fq_cas()
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn named_operations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let f1 = dir.path().join("f1.txt");
    let f2 = dir.path().join("f2.txt");
    std::fs::write(&f1, b"version one").unwrap();
    std::fs::write(&f2, b"version two").unwrap();
    let (f1s, f2s) = (f1.to_str().unwrap(), f2.to_str().unwrap());

    // name put -> cid; name get -> bytes; resolve -> same cid
    let out = run(root, &["name", "put", "docs.readme", f1s]);
    assert!(
        out.status.success(),
        "name put: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(cid.len(), 64);
    assert_eq!(
        run(root, &["name", "get", "docs.readme"]).stdout,
        b"version one"
    );
    assert_eq!(
        String::from_utf8_lossy(&run(root, &["name", "resolve", "docs.readme"]).stdout).trim(),
        cid
    );

    // a second name; ls the namespace (sorted)
    run(root, &["name", "put", "docs.guide", f1s]);
    assert_eq!(
        String::from_utf8_lossy(&run(root, &["name", "ls", "docs"]).stdout),
        "docs.guide\ndocs.readme\n"
    );

    // overwrite -> get reflects v2; history is newest-first, oldest == original
    run(root, &["name", "put", "docs.readme", f2s]);
    assert_eq!(
        run(root, &["name", "get", "docs.readme"]).stdout,
        b"version two"
    );
    let hist = String::from_utf8(run(root, &["name", "history", "docs.readme"]).stdout).unwrap();
    let versions: Vec<&str> = hist.lines().collect();
    assert_eq!(versions.len(), 2, "two versions in history");
    assert_eq!(versions[1], cid, "oldest version is last");

    // rm -> name gone, ls reflects it, get errors
    assert!(run(root, &["name", "rm", "docs.guide"]).status.success());
    assert_eq!(
        String::from_utf8_lossy(&run(root, &["name", "ls", "docs"]).stdout),
        "docs.readme\n"
    );
    assert!(
        !run(root, &["name", "get", "docs.guide"]).status.success(),
        "get of a removed name should fail"
    );
}
