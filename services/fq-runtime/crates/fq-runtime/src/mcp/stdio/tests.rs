//! Unit tests for [`super`]: what a stdio MCP server actually sees when
//! it starts (#541). Each test spawns the configured command itself —
//! `env`, `pwd` — and reads the answer back, so the assertions are about
//! the child's view and not about how the command was built.

use std::collections::BTreeMap;
use std::process::Stdio;

use super::*;

fn config(command: &str, env: &[(&str, &str)]) -> McpServerConfig {
    McpServerConfig {
        name: "probe".to_string(),
        command: command.to_string(),
        args: vec![],
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        url: None,
    }
}

async fn run(mut cmd: Command) -> String {
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn probe command");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 output")
}

/// The acceptance test for A2: the child's environment is *exactly* the
/// pinned `PATH` plus the declared pairs. Asserting equality on the whole
/// set is what proves nothing leaked from this process — whose own
/// environment certainly holds `HOME`, `PATH` and, under `just`, a broker
/// URL and token.
#[tokio::test]
async fn child_sees_exactly_the_pinned_path_and_the_declared_env() {
    assert!(
        std::env::var_os("HOME").is_some() || std::env::var_os("PATH").is_some(),
        "the test process must have an environment worth leaking"
    );
    let root = tempfile::tempdir().expect("tempdir");
    let cmd = stdio_command(
        &config(
            "env",
            &[("DECLARED_ONE", "1"), ("DECLARED_TWO", "two words")],
        ),
        &root.path().join("probe"),
    )
    .expect("env is on PATH");

    let seen: BTreeMap<String, String> = run(cmd)
        .await
        .lines()
        .map(|line| {
            let (k, v) = line.split_once('=').expect("KEY=VALUE");
            (k.to_string(), v.to_string())
        })
        .collect();

    let program = resolve_program("env").expect("env resolves");
    let expected: BTreeMap<String, String> = [
        ("PATH", child_path(&program)),
        ("DECLARED_ONE", "1".to_string()),
        ("DECLARED_TWO", "two words".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(seen, expected, "the child saw something it was not given");
    assert!(
        seen["PATH"].ends_with(DEFAULT_CHILD_PATH),
        "the child's PATH must end in the shared baseline: {}",
        seen["PATH"]
    );
}

/// The declared `env:` is applied last, so a definition that needs a
/// fuller `PATH` (a toolchain the baseline lacks) can say so explicitly —
/// the same opt-in `exec` offers through its allowlist.
#[tokio::test]
async fn declared_env_is_applied_over_the_baseline() {
    let root = tempfile::tempdir().expect("tempdir");
    let cmd = stdio_command(
        &config("env", &[("PATH", "/opt/tools/bin:/usr/bin:/bin")]),
        root.path(),
    )
    .expect("env is on PATH");
    let out = run(cmd).await;
    assert_eq!(out.trim(), "PATH=/opt/tools/bin:/usr/bin:/bin", "{out}");
}

/// The child's working directory is its own `<root>/<server>`, created
/// on demand — not wherever the daemon (here, the test runner) happens
/// to be.
#[tokio::test]
async fn child_starts_in_its_own_directory_not_the_daemons() {
    let root = tempfile::tempdir().expect("tempdir");
    let server_dir = root.path().join("nested").join("probe");
    assert!(!server_dir.exists(), "created on demand, so absent before");
    let cmd = stdio_command(&config("pwd", &[]), &server_dir).expect("pwd is on PATH");
    let seen = std::path::PathBuf::from(run(cmd).await.trim());

    assert_eq!(
        seen.canonicalize().expect("child cwd exists"),
        server_dir.canonicalize().expect("server dir was created")
    );
    assert_ne!(
        seen.canonicalize().unwrap(),
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        "the child must not start in the daemon's cwd"
    );
}

/// The server's name becomes its directory name, so a name shaped like a
/// path is refused *before* any directory is built or created — not at
/// tool discovery, by which point `../x` would already have made a cwd
/// outside the root.
#[tokio::test]
async fn a_path_shaped_server_name_is_refused_before_any_directory_exists() {
    let root = tempfile::tempdir().expect("tempdir");
    let escape = format!("../escape-{}", std::process::id());
    for name in [escape.as_str(), "/tmp/x", "a/b"] {
        let mut cfg = config("env", &[]);
        cfg.name = name.to_string();
        let err = match spawn_transport(&cfg, root.path()) {
            Err(err) => err,
            Ok(_) => panic!("{name}: a path-shaped server name was accepted"),
        };
        assert!(
            matches!(err, McpError::ToolDiscovery { .. }),
            "{name}: {err:?}"
        );
        assert!(
            !root.path().join(name).exists(),
            "{name}: a directory was created before the name was refused"
        );
    }
    assert!(
        std::fs::read_dir(root.path()).unwrap().next().is_none(),
        "the root must be untouched by refused names"
    );
}

/// The child's `PATH` admits the directory the command came from —
/// what lets `npx` find its sibling `node` under nvm, mise or a CI
/// toolcache — and nothing else beyond the baseline. A program already
/// on the baseline adds nothing.
#[test]
fn child_path_admits_only_the_programs_own_directory() {
    assert_eq!(child_path(Path::new("/usr/bin/env")), DEFAULT_CHILD_PATH);
    assert_eq!(
        child_path(Path::new("/opt/node/bin/npx")),
        format!("/opt/node/bin:{DEFAULT_CHILD_PATH}")
    );
}

/// A command that is not on the daemon's PATH fails at build time, by
/// name, so the `ServerStart` error an operator reads says which
/// `command:` was wrong.
#[test]
fn an_unknown_command_fails_by_name_before_anything_is_spawned() {
    let root = tempfile::tempdir().expect("tempdir");
    let err = stdio_command(
        &config("this-binary-does-not-exist-12345", &[]),
        root.path(),
    )
    .expect_err("must not resolve");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(
        err.to_string().contains("this-binary-does-not-exist-12345"),
        "{err}"
    );
}

/// A `command:` given as a relative path is pinned to an absolute one
/// before the child changes directory, so it still means the file the
/// author pointed at.
#[test]
fn a_relative_command_path_is_made_absolute_against_the_daemons_cwd() {
    let program = resolve_program("./target/../target")
        .err()
        .map(|e| e.kind());
    // Whatever `./target` is here, it is not an executable file — the
    // point is that a `/`-bearing name goes through the path branch and
    // yields NotFound rather than a PATH search.
    assert_eq!(program, Some(io::ErrorKind::NotFound));
    let env = resolve_program("env").expect("env resolves");
    assert!(env.is_absolute());
    let via_path = resolve_program(env.to_str().unwrap()).expect("absolute path resolves");
    assert_eq!(via_path, env);
}

/// A relative entry in the daemon's `PATH` (`.`, `bin`, or an empty
/// entry) finds a relative candidate; the answer is still absolute, so
/// the exec after `current_dir` and the directory `child_path` admits
/// both mean what the daemon's cwd meant.
#[cfg(unix)]
#[test]
fn a_relative_path_entry_still_yields_an_absolute_program() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let probe = dir.path().join("probe-rel");
    std::fs::write(&probe, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The temp dir, spelled relative to this process's cwd: up to `/`,
    // then down the temp dir's components.
    let cwd = std::env::current_dir().unwrap();
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    for component in dir.path().components().skip(1) {
        relative.push(component.as_os_str());
    }
    assert!(relative.is_relative(), "{}", relative.display());

    let found = resolve_program_in("probe-rel", &relative).expect("found via the relative entry");
    assert!(found.is_absolute(), "{}", found.display());
    assert_eq!(
        found.canonicalize().unwrap(),
        probe.canonicalize().unwrap(),
        "the absolute answer must be the same file"
    );
    assert!(
        child_path(&found).starts_with('/'),
        "the admitted directory must be absolute too: {}",
        child_path(&found)
    );
}

fn collect_lines(bytes: &'static [u8]) -> Vec<String> {
    let mut seen = Vec::new();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(forward_stderr(bytes, |line| seen.push(line)));
    seen
}

/// The stderr forwarder is byte-oriented: a line that is not UTF-8 is
/// rendered lossily and the lines after it still arrive (a `lines()`
/// loop stops at the first bad byte and drops the pipe, after which the
/// child takes `EPIPE` on every write). Long lines are cut at the cap and
/// say so; an unterminated last line is not lost.
#[test]
fn stderr_forwarding_survives_non_utf8_and_caps_long_lines() {
    let long = "x".repeat(STDERR_LINE_CAP + 100);
    let input: &'static [u8] = Box::leak(
        [
            &b"first\r\n"[..],
            b"\xff\xfe bad bytes\n",
            b"second\n",
            long.as_bytes(),
            b"\nlast without newline",
        ]
        .concat()
        .into_boxed_slice(),
    );
    let seen = collect_lines(input);
    assert_eq!(
        seen,
        vec![
            "first".to_string(),
            "\u{FFFD}\u{FFFD} bad bytes".to_string(),
            "second".to_string(),
            format!("{} …[100 more bytes dropped]", "x".repeat(STDERR_LINE_CAP)),
            "last without newline".to_string(),
        ]
    );
}

/// The same property over a real pipe: a child that writes a bad byte
/// and then a good line is still heard after the bad byte, and the
/// forwarder returns at EOF rather than hanging.
#[cfg(unix)]
#[tokio::test]
async fn stderr_forwarding_keeps_reading_a_real_pipe_after_a_bad_byte() {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf '\\377bad\\n' >&2; echo after >&2; exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut seen = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        forward_stderr(stderr, |line| seen.push(line)),
    )
    .await
    .expect("the forwarder must return at EOF");
    child.wait().await.expect("reap sh");
    assert_eq!(seen, vec!["\u{FFFD}bad".to_string(), "after".to_string()]);
}
