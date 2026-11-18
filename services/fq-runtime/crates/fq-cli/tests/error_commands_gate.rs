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

use std::collections::{BTreeSet, HashMap};
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
///
/// A span that opens with `fq` is read whole even where it runs onto the
/// next lines, as a long message does: a string continued with `\`, or
/// a comment wrapped mid-command ([`joined_lines`]). Every long flag in
/// it counts, not only those before the first placeholder or value: a
/// command such as `` `fq events query --agent {agent} --type triggered` ``
/// names `--type` after a value, and a gate that stopped at `{agent}`
/// passed it, though `fq events query` has no `--type`. A flag's name is
/// read whole, up to a value (`=`) or the punctuation that closes the
/// sentence, so `--event_type` is not taken for `--event`, and each of
/// `--a/--b` counts.
fn references_in(text: &str, file: &str) -> Vec<Reference> {
    let mut found = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (lineno, line) in joined_lines(&lines) {
        // An explicit exception, for prose that names a retired or
        // not-yet-built verb on purpose. Comments only: see the module
        // doc for why a string literal cannot claim it.
        let marked =
            |l: &str| l.contains("allow-dead-command:") && l.trim_start().starts_with("//");
        let prev_marked = lineno > 0 && marked(lines[lineno - 1]);
        if marked(lines[lineno]) || prev_marked {
            continue;
        }
        for span in line.split('`').skip(1).step_by(2) {
            let mut words = span.split_whitespace();
            if words.next() != Some("fq") {
                continue;
            }
            let mut path = Vec::new();
            let mut flags = BTreeSet::new();
            let mut verbs = true;
            for word in words {
                let named: Vec<&str> = word
                    .split('/')
                    .filter_map(|part| part.strip_prefix("--"))
                    .collect();
                if !named.is_empty() {
                    for flag in named {
                        // `--flag=value` and a trailing comma or stop
                        // both appear in real prose; keep the name, all
                        // of it.
                        let name = flag
                            .split('=')
                            .next()
                            .unwrap_or(flag)
                            .trim_end_matches(|c: char| ",.;:)]}'\"`".contains(c));
                        if !name.is_empty() {
                            flags.insert(name.to_string());
                        }
                    }
                    verbs = false;
                } else if verbs
                    && !word.is_empty()
                    && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                {
                    // Subcommands come before flags.
                    path.push(word.to_string());
                } else {
                    // A placeholder (`<id>`, `{agent}`), a path, a value
                    // or a quoted payload ends the path — none of them
                    // are verbs — but not the flags after it.
                    verbs = false;
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

/// Most lines one backticked span is read across, its first included.
const SPAN_LINES: usize = 5;

/// `lines` joined where a backticked `fq` command runs on, each with the
/// index of the line it starts on. A line that leaves a span open (an
/// odd number of backticks) whose open span starts with `fq` takes the
/// next, up to [`SPAN_LINES`] lines in all, less what only continues it:
/// a string's `\` and the next line's indent, or a comment's leader
/// (`///`, `//!`, `//`). Only such a span is followed. A line left odd by
/// anything else (a code fence, a `'`'` char) is read alone, as every
/// line was before spans were followed: joining it would pair the next
/// lines' backticks the wrong way round and hide a command among them.
fn joined_lines(lines: &[&str]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        let mut joined = lines[i].to_string();
        while opens_a_command(&joined) && i + 1 < lines.len() && i + 1 - start < SPAN_LINES {
            i += 1;
            let head = joined.trim_end();
            let head = head.strip_suffix('\\').unwrap_or(head).trim_end();
            joined = format!("{head} {}", continued(lines[i]));
        }
        out.push((start, joined));
        i += 1;
    }
    out
}

/// Whether `line` leaves a backticked span open, and that span is an
/// `fq` command.
fn opens_a_command(line: &str) -> bool {
    line.matches('`').count() % 2 == 1
        && line
            .rsplit('`')
            .next()
            .is_some_and(|open| open.split_whitespace().next() == Some("fq"))
}

/// What `line` adds to a span the line before left open: the line less
/// its indent and any comment leader.
fn continued(line: &str) -> &str {
    let line = line.trim_start();
    ["///", "//!", "//"]
        .iter()
        .find_map(|leader| line.strip_prefix(leader))
        .unwrap_or(line)
        .trim_start()
}

/// Whether `help` lists the long flag `--name`, the whole of it: not
/// `--event` for `--event-type`.
fn has_flag(help: &str, name: &str) -> bool {
    let needle = format!("--{name}");
    help.match_indices(&needle).any(|(at, _)| {
        help[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    })
}

/// The subcommands a `--help` lists, by name.
fn subcommands(help: &str) -> Vec<&str> {
    help.lines()
        .skip_while(|line| line.trim_end() != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .collect()
}

/// Words that read as a subcommand. Where a command takes a value
/// instead, `fq trigger get <id>` parses, and means "trigger the agent
/// named `get`": clap answers `--help` for it, so only this tells it
/// from a real command.
const VERB_LIKE: &[&str] = &[
    "get", "list", "show", "query", "describe", "stream", "tail", "status", "delete", "drop",
];

/// Each path's `--help`, asked once.
#[derive(Default)]
struct Helps(HashMap<Vec<String>, Option<String>>);

impl Helps {
    fn of(&mut self, path: &[String]) -> Option<&str> {
        self.0
            .entry(path.to_vec())
            .or_insert_with(|| help_for(path))
            .as_deref()
    }

    /// The first word of `path` that reads as a subcommand but where the
    /// command before it takes a value, with that command: the word is
    /// read as the value.
    fn misread(&mut self, path: &[String]) -> Option<(String, String)> {
        if path.first().is_some_and(|verb| verb == "help") {
            return None;
        }
        for at in 1..path.len() {
            let help = self.of(&path[..at])?.to_string();
            if subcommands(&help).contains(&path[at].as_str()) {
                continue;
            }
            return VERB_LIKE
                .contains(&path[at].as_str())
                .then(|| (path[..at].join(" "), path[at].clone()));
        }
        None
    }
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
    let mut helps = Helps::default();
    for r in &refs {
        if let Some((command, word)) = helps.misread(&r.path) {
            dead.push(format!(
                "  {} — `fq {command}` has no subcommand `{word}`: it takes `{word}` as a value",
                r.origin
            ));
            continue;
        }
        match helps.of(&r.path).map(str::to_string) {
            None => dead.push(format!(
                "  {} — `fq {}` is not a command",
                r.origin,
                r.path.join(" ")
            )),
            Some(help) => {
                for flag in &r.flags {
                    if !has_flag(&help, flag) {
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

#[test]
fn a_flag_after_a_value_or_a_placeholder_is_read() {
    let refs = references_in(
        "`fq events query --agent {agent} --type triggered`\n\
         `fq dead-letters requeue <agent> --trigger-seq 7`",
        "t.rs",
    );
    let flags = |i: usize| refs[i].flags.iter().cloned().collect::<Vec<_>>();
    assert_eq!(refs[0].path, ["events", "query"]);
    assert_eq!(flags(0), ["agent", "type"]);
    assert_eq!(refs[1].path, ["dead-letters", "requeue"]);
    assert_eq!(flags(1), ["trigger-seq"]);
}

#[test]
fn a_span_that_runs_onto_the_next_line_is_read_whole() {
    let text = "        \"list the runs with `fq events query --agent {agent} --type \\\n\
                \u{20}        triggered`, then `fq \\\n\
                \u{20}        events get <event-id>`\"\n\
                /// read it with `fq invocation\n\
                /// show --json`";
    let refs = references_in(text, "t.rs");
    let found: Vec<(Vec<String>, Vec<String>, String)> = refs
        .into_iter()
        .map(|r| (r.path, r.flags.into_iter().collect(), r.origin))
        .collect();
    assert_eq!(
        found,
        [
            (
                vec!["events".to_string(), "query".to_string()],
                vec!["agent".to_string(), "type".to_string()],
                "t.rs:1".to_string()
            ),
            (
                vec!["events".to_string(), "get".to_string()],
                vec![],
                "t.rs:1".to_string()
            ),
            (
                vec!["invocation".to_string(), "show".to_string()],
                vec!["json".to_string()],
                "t.rs:4".to_string()
            ),
        ]
    );
}

#[test]
fn a_stray_backtick_does_not_hide_a_command_after_it() {
    // A code fence longer than the span limit, and a backtick char, each
    // leave a line odd; neither opens a command, so neither is followed.
    let text = "// ```text\n\
                // one\n\
                // two\n\
                // three\n\
                // four\n\
                // five\n\
                // ```\n\
                // Then run `fq no-such-verb-a` to finish.\n\
                fn scratch(c: char) -> bool {\n\
                \u{20}   c == '`'\n\
                }\n\
                // Run `fq no-such-verb-c` next.";
    let paths: Vec<Vec<String>> = references_in(text, "t.rs")
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(paths, [["no-such-verb-a"], ["no-such-verb-c"]]);
}

#[test]
fn a_flag_name_is_read_and_matched_whole() {
    let refs = references_in(
        "`fq events query --event_type triggered`\n\
         `fq workers list --stale-only/--alive-only`",
        "t.rs",
    );
    let flags = |i: usize| refs[i].flags.iter().cloned().collect::<Vec<_>>();
    assert_eq!(flags(0), ["event_type"]);
    assert_eq!(flags(1), ["alive-only", "stale-only"]);

    let help = "      --event-type <type>\n      --agent <AGENT>\n";
    assert!(has_flag(help, "event-type"));
    assert!(has_flag(help, "agent"));
    assert!(
        !has_flag(help, "event"),
        "a prefix of a flag is not the flag"
    );
    assert!(!has_flag(help, "event_type"));
}

#[test]
fn a_subcommand_word_read_as_a_value_is_named() {
    let mut helps = Helps::default();
    let path = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    assert_eq!(
        helps.misread(&path(&["trigger", "get"])),
        Some(("trigger".to_string(), "get".to_string()))
    );
    assert_eq!(helps.misread(&path(&["events", "get"])), None);
    assert_eq!(
        helps.misread(&path(&["dead-letters", "requeue", "researcher"])),
        None
    );
}
