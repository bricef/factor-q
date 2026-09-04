//! Gate: what `fq init` scaffolds, `fqd` can read.
//!
//! The invariant is one sentence — *every config file `fq init` emits
//! parses as the config type the binary that reads it will parse it as*
//! — and it went unheld long enough for `fq init` to ship a template
//! declaring `[state]` twice, which is not TOML at all, into a file the
//! daemon does not even open. A fresh project was dead on arrival in
//! both directions at once, and every gate in the tree was green,
//! because nobody had ever fed the template to a parser.
//!
//! This is the daemon half. The client half is a unit test in
//! `fq-cli`'s `project.rs`: `fq` links no runtime to parse `fqd.toml`
//! with (its `thin_client_gate`), and `ClientConfig` is private to that
//! crate, so neither end can hold both. Splitting the assertion is the
//! cost of the split binaries; leaving either end unasserted is not.
//!
//! The template is read from `fq-cli`'s source tree at run time rather
//! than embedded, for the reason `just lint-sources` gives: a
//! compile-time embed of another crate's file is the same reach one
//! step removed, and it goes stale silently.
//!
//! Parsing alone is the weaker question, so the gate asks two. TOML
//! validity and type-correctness are what a duplicate table or a
//! `retention_days = "30"` trips. What they do *not* trip is a key the
//! config stopped reading: serde drops an unknown key without a word,
//! so a template teaching a field that has since been renamed parses
//! perfectly and teaches a lie. The gate therefore also asks which keys
//! were ignored, and insists the answer is none.
//!
//! A third test keeps the scaffolded agent and the scaffolded config in
//! step, because the model registry is a startup gate too: the project
//! is only usable if `fqd` will run in it.

use std::path::{Path, PathBuf};

use fq_runtime::Config;

/// One of `fq init`'s templates, in the crate that writes it.
fn template_path(name: &str) -> PathBuf {
    // crates/fq-daemon → crates/fq-cli.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fq-cli/src/templates")
        .join(name)
}

fn fqd_template_path() -> PathBuf {
    template_path("fqd.toml")
}

fn read_template(name: &str) -> String {
    let path = template_path(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_fqd_template() -> String {
    read_template("fqd.toml")
}

/// Deserialise into the real [`Config`], reporting every key the
/// deserialiser had no field for.
fn load(template: &str) -> (Config, Vec<String>) {
    let mut ignored = Vec::new();
    let config = serde_ignored::deserialize(toml::Deserializer::new(template), |path| {
        ignored.push(path.to_string())
    })
    .unwrap_or_else(|e| {
        panic!(
            "{} does not parse as fq_runtime::Config — `fq init` would scaffold a project \
             whose daemon refuses to start:\n{e}",
            fqd_template_path().display()
        )
    });
    (config, ignored)
}

/// The file as written: it must parse, and every key it sets must be
/// one the daemon acts on.
#[test]
fn the_daemon_template_parses_as_the_daemon_config() {
    let (config, ignored) = load(&read_fqd_template());

    assert!(
        ignored.is_empty(),
        "{} sets keys fq_runtime::Config does not read: {}\nA key serde drops is a setting \
         the operator will believe took effect.",
        fqd_template_path().display(),
        ignored.join(", "),
    );

    // The settings the template actually commits to. All load bearing on
    // day one: the broker URL must be credential-free (#540 — a URL with
    // userinfo is refused at startup), the token must be named by the
    // variable the README tells the user to export for the scaffolded
    // docker-compose.yml's broker, and the agents directory has to be
    // the one `fq init` created.
    assert_eq!(config.nats.url, "nats://localhost:4222");
    assert_eq!(config.nats.token_env.as_deref(), Some("FQ_NATS_TOKEN"));
    assert!(config.agents.directory.ends_with("agents"));
    config
        .validate()
        .expect("the scaffolded daemon config must pass the daemon's own validation");
}

/// The commented-out examples too. They are most of the file and all of
/// its teaching — an operator configures a daemon by uncommenting one —
/// so a key that drifted out from under an example fails a project just
/// as thoroughly as one that drifted out from under a live setting, and
/// a gate that only read the live settings would cover four keys out of
/// thirty.
#[test]
fn the_daemon_template_teaches_settings_the_daemon_reads() {
    let (config, ignored) = load(&with_examples_enabled(&read_fqd_template()));

    assert!(
        ignored.is_empty(),
        "{} documents keys fq_runtime::Config does not read: {}\nEither the example names a \
         field that was renamed or removed, or two examples set the same key and the second \
         wins — both mislead the reader.",
        fqd_template_path().display(),
        ignored.join(", "),
    );

    // Spot-check the values across the sections a reader is most
    // likely to uncomment, so the gate proves the settings *arrive*
    // rather than merely being tolerated.
    assert_eq!(config.max_iterations, 100);
    assert_eq!(config.state.retention_days, 30);
    assert_eq!(config.state.stale_worker_retention_days, 7);
    assert_eq!(config.state.sweep_interval_seconds, 3_600);
    assert_eq!(config.worker.max_concurrent_invocations, 1);
    assert_eq!(config.tools.exec.max_timeout_secs, 600);
    assert!(config.workspace.path.is_some());
    // The second provider is documented as configuration-only — no
    // code — so the example has to survive the registry's own shape.
    assert!(config.providers.extra.contains_key("openrouter"));
}

/// The scaffolded config and the scaffolded agent have to agree. The
/// model registry (ADR-0004) is a startup gate, not a warning: a
/// definition naming a model no provider declares makes `fqd` refuse
/// to start, so two templates that drift apart hand a new user a
/// project whose daemon will not run — the same failure as an
/// unparseable config, arriving one file later.
#[test]
fn the_sample_agent_names_a_model_the_daemon_template_declares() {
    let (config, _) = load(&read_fqd_template());
    let agent = fq_runtime::agent::definition::parse_agent(&read_template("sample-agent.md"))
        .unwrap_or_else(|e| panic!("templates/sample-agent.md does not parse: {e}"));

    let declared: Vec<&str> = config.providers.declared_models().collect();
    assert!(
        declared.contains(&agent.model()),
        "templates/sample-agent.md runs on `{}`, which templates/fqd.toml does not declare \
         under any [providers.<name>] models = [...]: {declared:?}",
        agent.model(),
    );
}

/// The template with its commented-out examples enabled and its prose
/// left alone. A commented line is an example iff what sits under the
/// marker is itself valid TOML — the parser's judgement rather than a
/// regex's guess at English.
fn with_examples_enabled(template: &str) -> String {
    template
        .lines()
        .map(|line| match line.strip_prefix("# ") {
            Some(rest) if toml::from_str::<toml::Table>(rest).is_ok() => rest,
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
