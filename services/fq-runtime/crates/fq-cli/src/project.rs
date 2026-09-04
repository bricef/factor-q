//! `fq init`: the offline verb that writes a new project's files.
//!
//! Split out of `lib.rs` (#189). The templates are embedded in the binary and
//! written verbatim, so a fresh checkout needs no network and no daemon.

use std::path::Path;

use anyhow::Context;

/// Template files embedded in the binary and written verbatim when
/// `fq init` runs.
///
/// Two config files, because there are two binaries: `fq` reads
/// [`FQ_TOML_TEMPLATE`], `fqd` reads [`FQD_TOML_TEMPLATE`], and neither
/// parser has ever understood the other's file. Scaffolding one file
/// for both left a fresh project with a daemon that found no config and
/// a client that choked on the one it did find.
const FQ_TOML_TEMPLATE: &str = include_str!("templates/fq.toml");

const FQD_TOML_TEMPLATE: &str = include_str!("templates/fqd.toml");

const README_TEMPLATE: &str = include_str!("templates/README.md");
const SAMPLE_AGENT_TEMPLATE: &str = include_str!("templates/sample-agent.md");
const DOCKER_COMPOSE_TEMPLATE: &str = include_str!("templates/docker-compose.yml");

/// Initialise a new factor-q project in the current working directory.
///
/// Writes five files (plus an `agents/` directory):
/// - `fq.toml` (the client's)
/// - `fqd.toml` (the daemon's)
/// - `README.md`
/// - `docker-compose.yml` (NATS with JetStream)
/// - `agents/sample-agent.md`
///
/// Errors and exits if any of the target files already exist, unless
/// `--force` is set.
pub(crate) fn init_project(force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let fq_toml = cwd.join("fq.toml");
    let fqd_toml = cwd.join("fqd.toml");
    let readme = cwd.join("README.md");
    let agents_dir = cwd.join("agents");
    let sample_agent = agents_dir.join("sample-agent.md");
    let docker_compose = cwd.join("docker-compose.yml");

    // Detect conflicts up front so the user sees all of them at once
    // rather than fixing them one by one.
    if !force {
        let mut conflicts: Vec<&Path> = Vec::new();
        if fq_toml.exists() {
            conflicts.push(&fq_toml);
        }
        if fqd_toml.exists() {
            conflicts.push(&fqd_toml);
        }
        if readme.exists() {
            conflicts.push(&readme);
        }
        if sample_agent.exists() {
            conflicts.push(&sample_agent);
        }
        if docker_compose.exists() {
            conflicts.push(&docker_compose);
        }
        if !conflicts.is_empty() {
            let listing = conflicts
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "the following files already exist:\n{listing}\n\n\
                 Use `fq init --force` to overwrite them."
            );
        }
    }

    std::fs::create_dir_all(&agents_dir)
        .with_context(|| format!("failed to create {}", agents_dir.display()))?;
    write_file(&fq_toml, FQ_TOML_TEMPLATE)?;
    write_file(&fqd_toml, FQD_TOML_TEMPLATE)?;
    write_file(&readme, README_TEMPLATE)?;
    write_file(&docker_compose, DOCKER_COMPOSE_TEMPLATE)?;
    write_file(&sample_agent, SAMPLE_AGENT_TEMPLATE)?;

    println!("Initialised factor-q project in {}", cwd.display());
    println!();
    println!("Created:");
    println!("  fq.toml            (the client's config)");
    println!("  fqd.toml           (the daemon's config)");
    println!("  README.md");
    println!("  docker-compose.yml");
    println!("  agents/");
    println!("  agents/sample-agent.md");
    println!();
    println!("Next steps:");
    println!("  1. Start NATS (JetStream) in the background:");
    println!("     docker compose up -d");
    println!("  2. Export your LLM provider API key, e.g.:");
    println!("     export ANTHROPIC_API_KEY='sk-ant-...'");
    println!("  3. Start the daemon from this directory, so it reads fqd.toml.");
    println!("     On first run it prints its certificate fingerprint and writes");
    println!("     the admin token — once, owner-only, never to stdout — to");
    println!("     <state>/edge/admin.token (the state dir is ~/.local/state/factor-q");
    println!("     unless fqd.toml's [state] directory or FQ_STATE_DIR says otherwise):");
    println!("     fqd");
    println!("  4. In another shell, pair with it (the edge listens on");
    println!("     127.0.0.1:9472 unless fqd.toml's [edge] bind says otherwise).");
    println!("     From a terminal it shows the fingerprint and asks you to confirm;");
    println!("     from a script add --fingerprint \"$(cat <state>/edge/fingerprint)\":");
    println!("     fq connect 127.0.0.1:9472 --token \"$(cat <state>/edge/admin.token)\"");
    println!("  5. Trigger the sample agent:");
    println!("     fq trigger sample-agent \"Say hello in one sentence.\"");
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClientConfig;

    /// The client half of the invariant `fq init` exists to hold:
    /// every config file it writes parses as the config type the
    /// binary that reads it will parse it as. The daemon half is in
    /// `fq-daemon/tests/init_templates_gate.rs`, because `fq` links no
    /// runtime to parse `fqd.toml` with (`tests/thin_client_gate.rs`)
    /// and `fqd` cannot see this crate's private types.
    ///
    /// As emitted the file sets nothing, which is the point: a project
    /// with one pairing has nothing to disambiguate. What matters is
    /// that it *parses* — an unreadable `fq.toml` is not a degraded
    /// client but a dead one, since every verb loads it before it does
    /// anything.
    #[test]
    fn the_client_template_parses_as_the_client_config() {
        // Display, not Debug: toml's error Debug carries the whole
        // source file, which buries the one line that failed.
        let config: ClientConfig = toml::from_str(FQ_TOML_TEMPLATE).unwrap_or_else(|e| {
            panic!("templates/fq.toml does not parse as ClientConfig — `fq init` would leave every fq command dead:\n{e}")
        });
        assert_eq!(
            config.daemon.addr, None,
            "a fresh project should name no daemon: `fq connect` has not run yet, so any \
             address here would be a guess the client then reports as configured"
        );
    }

    /// The setting the template teaches has to *arrive*, not merely
    /// parse. Asserting the value is what catches a rename: serde
    /// ignores a key no field claims, so a template still teaching
    /// `[daemon] addr` after that field was renamed would satisfy any
    /// check that only asked whether the file parsed.
    #[test]
    fn the_client_template_teaches_a_setting_the_client_reads() {
        let config: ClientConfig = toml::from_str(&with_examples_enabled(FQ_TOML_TEMPLATE))
            .unwrap_or_else(|e| {
                panic!("templates/fq.toml's examples do not parse as ClientConfig:\n{e}")
            });
        assert_eq!(
            config.daemon.addr.as_deref(),
            Some("127.0.0.1:9472"),
            "uncommenting the template's `[daemon] addr` example must select a daemon; \
             if it no longer does, the key was renamed or removed under the template"
        );
    }

    /// The template with its commented-out examples enabled and its
    /// prose left alone. A commented line is an example iff what sits
    /// under the marker is itself valid TOML — the parser's judgement
    /// rather than a regex's guess at English.
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
}
