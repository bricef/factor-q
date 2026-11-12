//! End-to-end test of the `fq-cas` binary: put a file, then read it back by
//! its content id, and check `has` / `size`.
#![cfg(feature = "cli")]

use std::path::PathBuf;
use std::process::{Command, Output};

/// The binary under test, without `RUST_LOG` or `FQ_CAS_ROOT`: with the
/// first, `fq-cas` traces to stderr, which a test compares exactly; the
/// second would give it a root the test did not choose.
fn fq_cas() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fq-cas"));
    command.env_remove("RUST_LOG").env_remove("FQ_CAS_ROOT");
    command
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

/// How a test tells `fq-cas` where its store root is.
#[derive(Clone, Copy, Debug)]
enum Given {
    /// `--root`, with the path as the test wrote it.
    Flag,
    /// `FQ_CAS_ROOT`.
    Env,
    /// Neither: the default, `.fq-cas` in the working directory.
    Default,
}

/// A store root that does not exist yet, under a scratch working directory
/// that every command runs in. A default root therefore lands in scratch
/// and never in the repository, and a command has left nothing on disk
/// exactly when the scratch directory is still empty.
#[derive(Debug)]
struct NewRoot {
    cwd: tempfile::TempDir,
    /// What `--root` or `FQ_CAS_ROOT` is set to; unused for the default.
    arg: PathBuf,
    /// The root as an absolute path, as `fq-cas` names it.
    path: PathBuf,
    given: Given,
}

impl NewRoot {
    /// A root `levels` below the scratch directory, named by an absolute
    /// path when `absolute`, by one relative to the working directory
    /// otherwise.
    fn new(given: Given, levels: &[&str], absolute: bool) -> Self {
        let cwd = tempfile::tempdir().unwrap();
        let base = cwd.path().canonicalize().unwrap();
        let relative: PathBuf = match given {
            Given::Default => PathBuf::from(".fq-cas"),
            Given::Flag | Given::Env => levels.iter().collect(),
        };
        let path = base.join(&relative);
        let arg = if absolute { path.clone() } else { relative };
        Self {
            cwd,
            arg,
            path,
            given,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = fq_cas();
        command.current_dir(self.cwd.path());
        match self.given {
            Given::Flag => {
                command.arg("--root").arg(&self.arg);
            }
            Given::Env => {
                command.env("FQ_CAS_ROOT", &self.arg);
            }
            Given::Default => {}
        }
        command.args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// The line a command that does not create a store prints for this
    /// root.
    fn refusal(&self) -> String {
        let source = match self.given {
            Given::Flag => "from --root",
            Given::Env => "from FQ_CAS_ROOT",
            Given::Default => "the default, in the working directory",
        };
        format!(
            "fq-cas: the store root {} ({source}) does not exist. Point --root or FQ_CAS_ROOT at \
             an existing store, or start a new one with `fq-cas object put` or `fq-cas grant add`.\n",
            self.path.display()
        )
    }

    fn left_nothing(&self) -> bool {
        std::fs::read_dir(self.cwd.path()).unwrap().next().is_none()
    }
}

/// One new root for each way of naming one: a single missing directory by
/// `--root`; three missing levels by a relative `--root`, which `fq-cas`
/// names as an absolute path; two by `FQ_CAS_ROOT`; and the default root.
fn new_roots() -> [NewRoot; 4] {
    [
        NewRoot::new(Given::Flag, &["store"], true),
        NewRoot::new(Given::Flag, &["a", "b", "c", "store"], false),
        NewRoot::new(Given::Env, &["a", "b", "store"], true),
        NewRoot::new(Given::Default, &[], true),
    ]
}

fn generated_private_key() -> String {
    let out = fq_cas().args(["key", "generate"]).output().unwrap();
    assert!(out.status.success(), "key generate: {out:?}");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("private:"))
        .expect("key generate prints a private key")
        .trim()
        .to_string()
}

/// Every command that opens a database, other than the two that can start
/// a store, refuses a root that does not exist: one line naming it and
/// where it came from, exit 1, and nothing created, whichever way the root
/// was named (<https://github.com/bricef/factor-q/issues/991>). Creating
/// it instead would answer for an empty store when the root is mistyped.
#[test]
fn every_database_command_but_object_put_and_grant_add_refuses_a_missing_root() {
    let private = generated_private_key();
    let cid = "ab".repeat(32);
    let commands: &[&[&str]] = &[
        &["object", "get", "docs.readme"],
        &["object", "ls"],
        &["object", "resolve", "docs.readme"],
        &["object", "history", "docs.readme"],
        &["object", "rm", "docs.readme"],
        &["object", "bind", "docs.alias", &cid],
        &["gc"],
        &["grant", "ls", "bob"],
        &["grant", "check", "bob", "read", "docs.readme"],
        &["grant", "rm", "1"],
        &["token", "mint", "bob", "--biscuit-private-key", &private],
    ];
    for root in new_roots() {
        for args in commands {
            let out = root.run(args);
            let context = format!(
                "fq-cas {} with {:?} {:?}",
                args.join(" "),
                root.given,
                root.arg
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                root.refusal(),
                "{context}"
            );
            assert_eq!(out.status.code(), Some(1), "{context}");
            assert!(out.stdout.is_empty(), "{context}: {out:?}");
            assert!(root.left_nothing(), "{context} created something");
        }
    }
}

/// `object put` and `grant add` create a root that does not exist, parents
/// included, as `put` does, whichever way it was named; the commands that
/// refuse a missing root then work against it.
#[test]
fn object_put_and_grant_add_create_a_missing_root() {
    let inputs = tempfile::tempdir().unwrap();
    let input = inputs.path().join("input.txt");
    std::fs::write(&input, b"the first thing in a new store").unwrap();
    let input = input.to_str().unwrap();

    for root in new_roots() {
        let context = format!("{:?} {:?}", root.given, root.arg);
        let out = root.run(&["object", "put", "docs.readme", input]);
        assert!(out.status.success(), "object put with {context}: {out:?}");
        assert!(root.path.join("index.db").is_file(), "{context}");
        let out = root.run(&["object", "ls"]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "docs.readme\n");
        assert!(out.status.success(), "object ls with {context}: {out:?}");
        assert!(root.run(&["gc"]).status.success(), "gc with {context}");
    }

    for root in new_roots() {
        let context = format!("{:?} {:?}", root.given, root.arg);
        let out = root.run(&["grant", "add", "bob", "read", "docs.*"]);
        assert!(out.status.success(), "grant add with {context}: {out:?}");
        assert!(root.path.join("grants.db").is_file(), "{context}");
        let out = root.run(&["grant", "check", "bob", "read", "docs.readme"]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "allowed\n");
        assert!(out.status.success(), "grant check with {context}: {out:?}");
    }
}

/// A creating command checks the arguments it can check without a store
/// before it creates one, so a mistyped argument against a new root prints
/// its own error, not a database's, and leaves nothing behind.
#[test]
fn a_creating_command_with_a_bad_argument_says_so_and_leaves_no_store() {
    let inputs = tempfile::tempdir().unwrap();
    let absent = inputs.path().join("absent.txt");
    let absent = absent.to_str().unwrap();
    let cases: &[(&[&str], &str)] = &[
        (
            &["object", "put", "docs.readme", absent],
            "No such file or directory (os error 2)",
        ),
        (
            &["grant", "add", "a.b", "read", "docs.*"],
            "'a.b' is not a valid agent id \
             (non-empty; no dots, wildcards, whitespace, or control characters)",
        ),
        (
            &["grant", "add", "bob", "fly", "docs.*"],
            "unknown verb 'fly' (read|write|delete|list|grant)",
        ),
        (
            &["grant", "add", "bob", "read", "*"],
            "the scope must be a name or a namespace like research.papers.*",
        ),
    ];
    for root in new_roots() {
        for (args, error) in cases {
            let out = root.run(args);
            let context = format!(
                "fq-cas {} with {:?} {:?}",
                args.join(" "),
                root.given,
                root.arg
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                format!("fq-cas: {error}\n"),
                "{context}"
            );
            assert_eq!(out.status.code(), Some(1), "{context}");
            assert!(root.left_nothing(), "{context} created something");
        }
    }
}

/// A root that is a file, or that has a file among its parents, is named in
/// the error of every command that opens a database, creating or not.
#[test]
fn a_root_that_cannot_be_a_directory_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let input = base.join("input.txt");
    std::fs::write(&input, b"content").unwrap();
    let file = base.join("file");
    std::fs::write(&file, b"not a store").unwrap();
    let under_file = file.join("store");

    let input = input.to_str().unwrap();
    let commands: &[&[&str]] = &[
        &["object", "put", "docs.readme", input],
        &["object", "ls"],
        &["grant", "add", "bob", "read", "docs.*"],
        &["grant", "ls", "bob"],
    ];
    for args in commands {
        let out = run(&file, args);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!(
                "fq-cas: the store root {} (from --root) is not a directory\n",
                file.display()
            ),
            "fq-cas {}",
            args.join(" ")
        );
        assert_eq!(out.status.code(), Some(1));

        let out = run(&under_file, args);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!(
                "fq-cas: cannot use the store root {} (from --root): \
                 Not a directory (os error 20)\n",
                under_file.display()
            ),
            "fq-cas {}",
            args.join(" ")
        );
        assert_eq!(out.status.code(), Some(1));
    }
    assert_eq!(std::fs::read(&file).unwrap(), b"not a store");
}

/// A creating command that cannot create the root, because its parent is
/// not writable, names the root it was making.
#[cfg(unix)]
#[test]
fn a_root_that_cannot_be_created_is_named() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let input = base.join("input.txt");
    std::fs::write(&input, b"content").unwrap();
    let parent = base.join("read-only");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::create_dir(parent.join("probe")).is_ok() {
        // Running as root, which permissions do not stop: nothing to test.
        eprintln!("skipped: {} is writable at mode 555", parent.display());
        return;
    }

    let root = parent.join("store");
    let input = input.to_str().unwrap();
    let commands: &[&[&str]] = &[
        &["object", "put", "docs.readme", input],
        &["grant", "add", "bob", "read", "docs.*"],
    ];
    for args in commands {
        let out = run(&root, args);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!(
                "fq-cas: cannot create the store root {} (from --root): \
                 Permission denied (os error 13)\n",
                root.display()
            ),
            "fq-cas {}",
            args.join(" ")
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(!root.exists());
    }
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The walk-through in `docs/guide/access-control.md`, "Managing grants and
/// tokens", in an empty directory, where it starts a new store at the
/// default root. Each step is one `$` line of the guide, in the guide's
/// order, and `store.keys` is written and read as the shell would; a change
/// to either should be made to both.
#[test]
fn the_access_control_guides_walk_through_runs_on_a_new_store() {
    let root = NewRoot::new(Given::Default, &[], true);
    let keys = root.cwd.path().join("store.keys");
    let stdout = |out: &Output| String::from_utf8_lossy(&out.stdout).into_owned();

    // $ fq-cas key generate > store.keys
    let out = root.run(&["key", "generate"]);
    assert!(out.status.success(), "key generate: {out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Keep the private key secret (e.g. in FQ_BISCUIT_PRIVATE_KEY); \
         share only the public key with verifiers.\n"
    );
    std::fs::write(&keys, &out.stdout).unwrap();
    assert!(!root.path.exists(), "key generate touches no store");
    // $ chmod 600 store.keys
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // $ cat store.keys
    let pair = std::fs::read_to_string(&keys).unwrap();
    let lines: Vec<&str> = pair.lines().collect();
    assert_eq!(lines.len(), 2, "{pair}");
    assert!(lines[0].starts_with("private: "), "{pair}");
    assert!(lines[1].starts_with("public:  "), "{pair}");

    // $ fq-cas grant add bob read,write 'research.papers.*'
    let out = root.run(&["grant", "add", "bob", "read,write", "research.papers.*"]);
    assert!(out.status.success(), "grant add: {out:?}");
    assert_eq!(stdout(&out), "1\n");
    // $ fq-cas grant ls bob
    let out = root.run(&["grant", "ls", "bob"]);
    assert_eq!(stdout(&out), "1\tread,write\tresearch.papers.*\n");
    // $ fq-cas grant check bob read research.papers.doc1
    let out = root.run(&["grant", "check", "bob", "read", "research.papers.doc1"]);
    assert_eq!(
        (stdout(&out), out.status.code()),
        ("allowed\n".into(), Some(0))
    );

    // $ export FQ_BISCUIT_PRIVATE_KEY=$(awk '/^private:/ {print $2}' store.keys)
    // $ export FQ_BISCUIT_PUBLIC_KEY=$(awk '/^public:/ {print $2}' store.keys)
    let awk = |label: &str| {
        pair.lines()
            .filter(|line| line.starts_with(label))
            .find_map(|line| line.split_whitespace().nth(1))
            .unwrap()
            .to_string()
    };
    let (private, public) = (awk("private:"), awk("public:"));
    let exported = |args: &[&str]| {
        root.command(args)
            .env("FQ_BISCUIT_PRIVATE_KEY", &private)
            .env("FQ_BISCUIT_PUBLIC_KEY", &public)
            .output()
            .unwrap()
    };
    // $ token=$(fq-cas token mint bob --ttl 300)
    let out = exported(&["token", "mint", "bob", "--ttl", "300"]);
    assert!(out.status.success(), "token mint: {out:?}");
    let token = stdout(&out).trim_end_matches('\n').to_string();
    // $ fq-cas token attenuate "$token" --scope 'research.papers.reviews.*' --verbs read
    let out = exported(&[
        "token",
        "attenuate",
        &token,
        "--scope",
        "research.papers.reviews.*",
        "--verbs",
        "read",
    ]);
    assert!(out.status.success(), "token attenuate: {out:?}");
    let narrower = stdout(&out).trim_end_matches('\n').to_string();
    assert!(!narrower.is_empty() && narrower != token);
    // $ fq-cas token inspect "$token"
    let out = exported(&["token", "inspect", &token]);
    assert_eq!(stdout(&out), "principal: bob\n");

    // $ fq-cas grant rm 1
    let out = exported(&["grant", "rm", "1"]);
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    // $ fq-cas grant check bob read research.papers.doc1
    let out = exported(&["grant", "check", "bob", "read", "research.papers.doc1"]);
    assert_eq!(
        (stdout(&out), out.status.code()),
        ("denied\n".into(), Some(1))
    );
}
