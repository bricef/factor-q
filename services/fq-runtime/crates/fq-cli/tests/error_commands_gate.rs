//! Gate: every `fq` command this codebase names in its own prose must
//! actually parse.
//!
//! Three did not, and each was found in the worst possible place — the
//! error an operator reads when they are already stuck. `fq recovery`
//! and `fq recover` never existed; `fq invocation drop
//! --schema-mismatch` named a flag that was never added. The
//! schema-mismatch one fired on an incompatible store, so the runtime
//! met an operator mid-incident and handed them a command that does
//! nothing.
//!
//! These rot silently because nothing links a string literal to the
//! CLI. A verb gets renamed, the message keeps the old name, and the
//! only reader who finds out is someone having a bad day.
//!
//! So this checks the strings against the binary itself — not against a
//! list maintained here, which would be one more thing to drift.
//! `CARGO_BIN_EXE_fq` is the real client, and `--help` is clap's own
//! account of what it accepts. If a verb is renamed, this fails on the
//! next run without anyone remembering to update it.
//!
//! **Scope.** Every tracked `.rs` file in the workspace, comments
//! included. A comment naming a dead verb is cheaper to be wrong than
//! an error string, but it is the same decay and it misleads the next
//! maintainer, which is how the error strings got written.
//!
//! **Naming a retired command on purpose.** Prose legitimately says
//! things like "used to need `fq workers prune`" or "any future `fq
//! costs show`" — that is accurate history, or an honest forward
//! reference, and a gate that failed it would push people to delete
//! true statements. Mark those `allow-dead-command: <why>`, the same
//! escape `store_open_gate` uses and for the same reason: the exception
//! becomes a reviewable, greppable act rather than a silent one. The
//! marker is deliberately not available inside a string literal — an
//! operator reading an error cannot see your justification.
//!
//! Put the marker on its own `//` line above the reference, not inside
//! the `///` doc. Doc comments on schema'd types are *published* — they
//! travel in `describe`, print in `fq ops list`, and reach a model
//! through the MCP face — so a marker written there ships build-time
//! metadata to clients. The snapshot test caught exactly that on the
//! first attempt at this gate.

use std::collections::BTreeSet;
use std::process::Command;

/// A `fq …` reference lifted from source, with where it came from so a
/// failure names the line rather than the string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Reference {
    /// Subcommand path — `["invocation", "drop"]`.
    path: Vec<String>,
    /// Long flags named alongside it.
    flags: BTreeSet<String>,
    origin: String,
}

/// Pull `fq …` references out of one file's text.
///
/// Only backticked spans count. Prose in this codebase quotes commands
/// as `` `fq invocation list` ``, and requiring the backticks keeps the
/// scan away from ordinary sentences that happen to contain "fq".
fn references_in(text: &str, file: &str) -> Vec<Reference> {
    let mut found = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        // An explicit exception, for prose that names a retired or
        // not-yet-built verb on purpose. Comments only: see the module
        // doc for why a string literal cannot claim it.
        let marked =
            |l: &str| l.contains("allow-dead-command:") && l.trim_start().starts_with("//");
        let prev_marked = lineno > 0 && marked(text.lines().nth(lineno - 1).unwrap_or(""));
        if marked(line) || prev_marked {
            continue;
        }
        for span in line.split('`').skip(1).step_by(2) {
            let mut words = span.split_whitespace();
            if words.next() != Some("fq") {
                continue;
            }
            let mut path = Vec::new();
            let mut flags = BTreeSet::new();
            for word in words {
                if let Some(flag) = word.strip_prefix("--") {
                    // `--flag=value` and a trailing comma both appear in
                    // real prose; keep the name only.
                    let name = flag.split(['=', ',', '.']).next().unwrap_or(flag);
                    if !name.is_empty() {
                        flags.insert(name.to_string());
                    }
                } else if word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                    && !word.is_empty()
                    && flags.is_empty()
                {
                    // Subcommands come before flags. A placeholder
                    // (`<id>`), a path, or a quoted payload ends the
                    // path — none of them are verbs.
                    path.push(word.to_string());
                } else {
                    break;
                }
            }
            if !path.is_empty() {
                found.push(Reference {
                    path,
                    flags,
                    origin: format!("{file}:{}", lineno + 1),
                });
            }
        }
    }
    found
}

/// `--help` for a subcommand path, or `None` if clap rejects the path.
fn help_for(path: &[String]) -> Option<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(path)
        .arg("--help")
        .output()
        .expect("run fq");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn every_fq_command_named_in_source_actually_parses() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("workspace root")
        .to_path_buf();

    // `git ls-files` for the same reason fq-lint uses it: it is the set
    // that ships. An untracked scratch file is not this gate's business.
    let listing = Command::new("git")
        .args(["ls-files", "*.rs"])
        .current_dir(&root)
        .output()
        .expect("git ls-files");
    assert!(listing.status.success(), "git ls-files failed");

    let mut refs: Vec<Reference> = Vec::new();
    for rel in String::from_utf8_lossy(&listing.stdout).lines() {
        // This file quotes dead commands on purpose, in the module doc
        // above, to say why the gate exists.
        if rel.ends_with("error_commands_gate.rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        refs.extend(references_in(&text, rel));
    }
    assert!(
        refs.len() > 20,
        "expected the scan to find many `fq …` references; got {} — the \
         extractor is probably broken, and a gate that reads nothing passes \
         everything",
        refs.len()
    );

    let mut dead = Vec::new();
    for r in &refs {
        match help_for(&r.path) {
            None => dead.push(format!(
                "  {} — `fq {}` is not a command",
                r.origin,
                r.path.join(" ")
            )),
            Some(help) => {
                for flag in &r.flags {
                    if !help.contains(&format!("--{flag}")) {
                        dead.push(format!(
                            "  {} — `fq {}` has no `--{}`",
                            r.origin,
                            r.path.join(" "),
                            flag
                        ));
                    }
                }
            }
        }
    }

    assert!(
        dead.is_empty(),
        "source names {} `fq` command(s) that do not parse. An operator \
         following one of these gets nothing — and the error strings are \
         read at exactly the moment that hurts most:\n{}",
        dead.len(),
        dead.join("\n")
    );
}
