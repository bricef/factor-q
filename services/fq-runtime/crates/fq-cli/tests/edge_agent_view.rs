//! The Agent view over the edge (plan Phase 4, verb 9): Get answers
//! with one definition in full, List with the registry snapshot's
//! index — the definitions the daemon loaded and the files it
//! rejected.
//!
//! The point of the view is *whose* registry answers. `fq agent list`
//! used to read the caller's own agents directory, which is a
//! different question from the one being asked: the daemon holds its
//! registry in memory and `fq reload` swaps it, so the disk and the
//! running system routinely disagree. Here the daemon's directory is
//! the only one that exists, and the assertions are about the
//! registry it built from it.
//!
//! The load-error row is the part worth testing deliberately. A file
//! that fails to parse produces no agent and has no id, so it cannot
//! be a normal index row — and it is the row an operator most needs,
//! because it is the agent they expect to be running and is not.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

use fq_ops::{Domain, OpId};
use serde_json::json;

/// The model the fixture's definitions name, declared with an explicit
/// price so the daemon's pricing guarantee (ADR-0004) is satisfied
/// without reaching the network.
const MODEL: &str = "claude-haiku-4-5";

fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("edge-agent-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(
        dir.join("fq.toml"),
        format!(
            "[edge]\nbind = \"127.0.0.1:0\"\n\n[providers.anthropic]\nmodels = [\"{MODEL}\"]\n\n\
             [providers.anthropic.pricing.\"{MODEL}\"]\ninput_per_mtok = 1.0\n\
             output_per_mtok = 5.0\n"
        ),
    )
    .unwrap();

    // Two definitions the registry loads and one file it rejects.
    std::fs::write(
        dir.join("agents/probe.md"),
        format!(
            "---\nname: probe\nmodel: {MODEL}\ntools:\n  - builtin__exec\nbudget: 0.25\n\
             effort: high\n---\n\nYou are a probe.\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("agents/second.md"),
        format!("---\nname: second\nmodel: {MODEL}\n---\n\nYou are second.\n"),
    )
    .unwrap();
    std::fs::write(dir.join("agents/notes.md"), "# not a definition\n").unwrap();
    dir
}

fn suffix_of<'a>(log: &'a str, prefix: &str) -> &'a str {
    log.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .unwrap_or_else(|| panic!("log lacks prefix {prefix:?}\n--- log ---\n{log}"))
        .trim()
}

fn parse_fingerprint(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex fingerprint");
    }
    out
}

#[tokio::test]
async fn the_agent_view_answers_from_the_daemons_live_registry() {
    let server = fq_test_support::NatsServer::start();
    let scratch = unique_scratch();

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", server.url())
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("ANTHROPIC_API_KEY", "test-key-unused-by-this-test")
        .env("RUST_LOG", "off")
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
        assert!(tokio::time::Instant::now() < deadline, "fqd never ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let fingerprint = parse_fingerprint(suffix_of(
        &text,
        "edge: certificate fingerprint (clients pin this): ",
    ));
    let token = {
        let mut lines = text.lines();
        lines.find(|l| l.contains("edge: admin token")).unwrap();
        lines.next().unwrap().trim().to_string()
    };
    let addr = suffix_of(&text, "- edge is listening on ").to_string();
    // The daemon's own count of what it loaded, from its startup
    // banner: the view must agree with it, not merely with the files.
    assert!(
        text.contains("agents loaded:    2 (errors: 1)"),
        "the daemon should have loaded two definitions and rejected one\n--- log ---\n{text}"
    );

    let client = fq_edge::EdgeClient::connect(&addr, fingerprint, &token)
        .await
        .expect("connect edge");

    let invoke = |op: OpId, input: serde_json::Value| {
        let client = &client;
        async move {
            client
                .rpc
                .invoke(
                    tarpc::context::current(),
                    fq_edge::InvokeRequest {
                        op,
                        version: 1,
                        input,
                        min_seq: None,
                    },
                )
                .await
                .expect("rpc")
        }
    };

    // List: loaded definitions in id order, then the rejected file.
    let index = invoke(OpId::List(Domain::Agent), json!({}))
        .await
        .expect("agent.list")
        .output;
    let rows = index.as_array().expect("List answers with an array");
    assert_eq!(rows.len(), 3, "two agents and one rejected file: {index}");
    assert_eq!(rows[0]["entry"], "agent");
    assert_eq!(rows[0]["agent_id"], "probe");
    assert_eq!(rows[0]["model"], MODEL);
    assert_eq!(rows[0]["tool_count"], 1);
    assert_eq!(rows[1]["entry"], "agent");
    assert_eq!(rows[1]["agent_id"], "second");
    // The index row carries the file, so a listing answers "which
    // definition is this?" without a Get per row.
    assert!(
        rows[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("probe.md"),
        "{index}"
    );
    // The rejected file rides the index as its own kind of row.
    assert_eq!(rows[2]["entry"], "load_error");
    let message = rows[2]["message"].as_str().expect("message");
    assert!(
        message.contains("notes.md") && message.contains("frontmatter"),
        "the load error must name the file and the reason: {message}"
    );

    // Get: the definition in full, prompt included.
    let detail = invoke(OpId::Get(Domain::Agent), json!({ "agent_id": "probe" }))
        .await
        .expect("agent.get")
        .output;
    assert_eq!(detail["agent_id"], "probe");
    assert_eq!(detail["model"], MODEL);
    assert_eq!(detail["system_prompt"], "You are a probe.");
    assert_eq!(detail["tools"][0], "builtin__exec");
    assert_eq!(detail["budget"], 0.25);
    assert_eq!(detail["effort"], "high");
    assert!(
        detail["path"].as_str().expect("path").ends_with("probe.md"),
        "{detail}"
    );

    // An id nobody defined, and an id no agent could have: both are
    // NotFound. A malformed id cannot name a registry entry, so it is
    // absence, not a bad request.
    for missing in ["no-such-agent", "NOT A VALID ID!!"] {
        let err = invoke(OpId::Get(Domain::Agent), json!({ "agent_id": missing }))
            .await
            .expect_err("unknown agent must be NotFound");
        assert!(
            matches!(err, fq_edge::wire::WireError::NotFound { .. }),
            "expected NotFound for {missing}, got {err:?}"
        );
    }

    // The rejected file is not addressable as an agent — it never
    // became one, and the index row says so instead.
    let err = invoke(OpId::Get(Domain::Agent), json!({ "agent_id": "notes" }))
        .await
        .expect_err("a rejected definition is not an agent");
    assert!(matches!(err, fq_edge::wire::WireError::NotFound { .. }));

    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}
