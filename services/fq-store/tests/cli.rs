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

    // object put -> cid; object get -> bytes; resolve -> same cid
    let out = run(root, &["object", "put", "docs.readme", f1s]);
    assert!(
        out.status.success(),
        "object put: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(cid.len(), 64);
    assert_eq!(
        run(root, &["object", "get", "docs.readme"]).stdout,
        b"version one"
    );
    assert_eq!(
        String::from_utf8_lossy(&run(root, &["object", "resolve", "docs.readme"]).stdout).trim(),
        cid
    );

    // a second name; ls the namespace (sorted)
    run(root, &["object", "put", "docs.guide", f1s]);
    assert_eq!(
        String::from_utf8_lossy(&run(root, &["object", "ls", "docs"]).stdout),
        "docs.guide\ndocs.readme\n"
    );

    // overwrite -> get reflects v2; history is newest-first, oldest == original
    run(root, &["object", "put", "docs.readme", f2s]);
    assert_eq!(
        run(root, &["object", "get", "docs.readme"]).stdout,
        b"version two"
    );
    let hist = String::from_utf8(run(root, &["object", "history", "docs.readme"]).stdout).unwrap();
    let versions: Vec<&str> = hist.lines().collect();
    assert_eq!(versions.len(), 2, "two versions in history");
    assert_eq!(versions[1], cid, "oldest version is last");

    // rm -> name gone, ls reflects it, get errors
    assert!(run(root, &["object", "rm", "docs.guide"]).status.success());
    assert_eq!(
        String::from_utf8_lossy(&run(root, &["object", "ls", "docs"]).stdout),
        "docs.readme\n"
    );
    assert!(
        !run(root, &["object", "get", "docs.guide"]).status.success(),
        "get of a removed name should fail"
    );
}

#[test]
fn gc_reclaims_unreferenced_and_spares_live() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let keep = dir.path().join("keep.bin");
    let drop = dir.path().join("drop.bin");
    std::fs::write(&keep, vec![1u8; 20_000]).unwrap();
    std::fs::write(&drop, vec![2u8; 20_000]).unwrap();

    run(root, &["object", "put", "keep", keep.to_str().unwrap()]);
    run(root, &["object", "put", "drop", drop.to_str().unwrap()]);
    // Remove one name → its object is now unreferenced (dead).
    assert!(run(root, &["object", "rm", "drop"]).status.success());

    // gc reclaims the dead object and reports no alarms.
    let out = run(root, &["gc"]);
    assert!(
        out.status.success(),
        "gc: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("reclaimed objects"), "gc report: {text}");
    assert!(
        text.contains("alarms                none"),
        "gc report: {text}"
    );

    // The live name survives untouched.
    assert_eq!(
        run(root, &["object", "get", "keep"]).stdout,
        vec![1u8; 20_000]
    );

    // A second gc is a clean no-op — nothing left to reclaim, still no alarms.
    let out = run(root, &["gc", "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"reclaimed_objects\": 0"),
        "second gc: {json}"
    );
    assert!(json.contains("\"alarms\": []"), "second gc: {json}");
}

#[test]
fn access_control_operator_flow() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // key generate -> a hex keypair on stdout.
    let out = run(root, &["key", "generate"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let mut private = None;
    let mut public = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("private:") {
            private = Some(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("public:") {
            public = Some(rest.trim().to_string());
        }
    }
    let (private, public) = (private.unwrap(), public.unwrap());

    // grant add -> prints the grant id; ls shows it; check allows.
    let out = run(
        root,
        &["grant", "add", "bob", "read,write", "research.papers.*"],
    );
    assert!(
        out.status.success(),
        "grant add: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let ls = String::from_utf8(run(root, &["grant", "ls", "bob"]).stdout).unwrap();
    assert!(ls.contains(&id) && ls.contains("read,write") && ls.contains("research.papers.*"));
    assert!(
        run(
            root,
            &["grant", "check", "bob", "read", "research.papers.doc1"]
        )
        .status
        .success()
    );
    // Outside the scope, and a dotted agent id: refused.
    assert!(
        !run(root, &["grant", "check", "bob", "read", "docs.readme"])
            .status
            .success()
    );
    assert!(
        !run(root, &["grant", "add", "a.b", "read", "docs.*"])
            .status
            .success()
    );

    // token mint (key via flag) -> verify + inspect round-trips the principal.
    let out = run(
        root,
        &["token", "mint", "bob", "--biscuit-private-key", &private],
    );
    assert!(
        out.status.success(),
        "mint: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let out = run(
        root,
        &["token", "inspect", &token, "--biscuit-public-key", &public],
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("principal: bob"));

    // Attenuate to read-only on a narrower scope: still a valid token.
    let out = run(
        root,
        &[
            "token",
            "attenuate",
            &token,
            "--scope",
            "research.papers.reviews.*",
            "--verbs",
            "read",
            "--biscuit-public-key",
            &public,
        ],
    );
    assert!(
        out.status.success(),
        "attenuate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let narrowed = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_ne!(narrowed, token);
    let out = run(
        root,
        &[
            "token",
            "inspect",
            &narrowed,
            "--biscuit-public-key",
            &public,
        ],
    );
    assert!(out.status.success());

    // grant rm -> revocation is immediate: check flips to denied.
    assert!(run(root, &["grant", "rm", &id]).status.success());
    assert!(
        !run(
            root,
            &["grant", "check", "bob", "read", "research.papers.doc1"]
        )
        .status
        .success()
    );
    // Removing a nonexistent grant fails loudly.
    assert!(!run(root, &["grant", "rm", "9999"]).status.success());
}

/// The `user_version` of the SQLite file at `db`, set to `stamp` first when
/// one is given: what a newer `fq-cas`'s upgrade would have recorded.
fn user_version(db: &std::path::Path, stamp: Option<i64>) -> i64 {
    use sqlx::{ConnectOptions, Connection};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(db)
            .connect()
            .await
            .unwrap();
        if let Some(version) = stamp {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "PRAGMA user_version = {version}"
            )))
            .execute(&mut conn)
            .await
            .unwrap();
        }
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        version
    })
}

/// A store a newer `fq-cas` upgraded is refused by every command that opens
/// one of its databases, the read-only ones included: each prints the
/// refusal on one line, naming the file and both versions, and exits 1,
/// and the file keeps the version the newer binary recorded
/// (<https://github.com/bricef/factor-q/issues/984>).
#[test]
fn every_command_refuses_a_database_a_newer_fq_cas_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let input = dir.path().join("input.txt");
    std::fs::write(&input, b"written before the upgrade").unwrap();
    let input = input.to_str().unwrap();
    let out = run(root, &["object", "put", "docs.readme", input]);
    assert!(out.status.success(), "object put: {out:?}");
    let cid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(
        run(root, &["grant", "add", "bob", "read", "docs.*"])
            .status
            .success()
    );
    let keys = String::from_utf8(run(root, &["key", "generate"]).stdout).unwrap();
    let private = keys
        .lines()
        .find_map(|line| line.strip_prefix("private:"))
        .expect("key generate prints a private key")
        .trim()
        .to_string();

    let index: &[&[&str]] = &[
        &["object", "get", "docs.readme"],
        &["object", "ls"],
        &["object", "resolve", "docs.readme"],
        &["object", "history", "docs.readme"],
        &["object", "put", "docs.other", input],
        &["object", "bind", "docs.alias", &cid],
        &["object", "rm", "docs.readme"],
        &["gc"],
    ];
    let grants: &[&[&str]] = &[
        &["grant", "ls", "bob"],
        &["grant", "check", "bob", "read", "docs.readme"],
        &["grant", "add", "carol", "read", "docs.*"],
        &["grant", "rm", "1"],
        &["token", "mint", "bob", "--biscuit-private-key", &private],
    ];
    for (file, commands) in [("index.db", index), ("grants.db", grants)] {
        let db = root.join(file);
        let supported = user_version(&db, None);
        let newer = user_version(&db, Some(supported + 1));
        let refusal = format!(
            "fq-cas: incompatible schema: {} is at version {newer}, this binary supports up to \
             {supported}. The file was written by a newer fq-cas: run an fq-cas at least as new \
             as that one against this store.\n",
            db.display()
        );
        for args in commands {
            let out = run(root, args);
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                refusal,
                "fq-cas {}",
                args.join(" ")
            );
            assert_eq!(out.status.code(), Some(1), "fq-cas {}", args.join(" "));
            assert!(out.stdout.is_empty(), "fq-cas {}: {out:?}", args.join(" "));
        }
        assert_eq!(
            user_version(&db, None),
            newer,
            "a refused command must leave {file} at the newer version"
        );
    }
}
