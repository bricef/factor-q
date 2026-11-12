//! Gate: `fq-cas` opens its databases only through `src/cli/root.rs`.
//!
//! SQLite creates a database file but never the directory it goes in, so
//! opening `index.db` or `grants.db` under a store root that does not exist
//! fails with a line that names nothing
//! (<https://github.com/bricef/factor-q/issues/991>). `cli/root.rs` is where
//! a command says what it does about a missing root, create or refuse, and
//! its two openers take that decision as an argument. That holds only while
//! nothing else in the CLI opens a database: `cli.rs` has the root path and
//! both database types in scope, so a new command written as
//! `SqliteNameIndex::open(cli.root.join("index.db"))` would compile, pass
//! every behavioural test that does not exercise it against a missing root,
//! and bring the bare SQLite error back. This scan refuses that line.
//!
//! The sources are read at runtime rather than embedded, for the reason
//! `just lint-sources` gives. Comment lines are skipped on both sides, so a
//! doc comment may name the openers and cannot stand in for a real call in
//! `root.rs`. Test modules are not scanned: a unit test may open a database
//! directly. The scan matches the plain paths only; an alias or a
//! `<SqliteNameIndex>::open` would pass it, but that is a deliberate way
//! round it, not the accidental regression it is here to stop.

use std::path::{Path, PathBuf};

/// The two library calls that open a store database from a file path.
const OPENS: &[&str] = &["SqliteNameIndex::open", "SqliteGrantLog::open"];

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// The code lines of `file` (comment lines dropped) that call an opener,
/// with their line numbers.
fn opens_in(file: &Path) -> Vec<(usize, String)> {
    let text =
        std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| OPENS.iter().any(|open| line.contains(open)))
        .map(|(i, line)| (i + 1, line.trim().to_string()))
        .collect()
}

#[test]
fn the_cli_opens_its_databases_only_through_cli_root() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let root_module = src.join("cli").join("root.rs");
    let is_test_module = |path: &Path| {
        path.components()
            .any(|part| part.as_os_str() == "tests" || part.as_os_str() == "tests.rs")
    };

    let mut files = vec![src.join("cli.rs")];
    rust_files(&src.join("cli"), &mut files);
    let scanned: Vec<PathBuf> = files
        .into_iter()
        .filter(|path| *path != root_module && !is_test_module(path))
        .collect();
    assert!(
        scanned.contains(&src.join("cli.rs")),
        "the gate scanned nothing it was meant to: {scanned:?}"
    );

    let offenders: Vec<String> = scanned
        .iter()
        .flat_map(|file| {
            opens_in(file)
                .into_iter()
                .map(move |(n, line)| format!("  {}:{n}: {line}", file.display()))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "fq-cas opens index.db and grants.db only through {}, which decides what a \
         command does about a missing store root; open them with StoreRoot::repository \
         or StoreRoot::grant_log instead:\n{}",
        root_module.display(),
        offenders.join("\n")
    );

    let in_root = opens_in(&root_module);
    for open in OPENS {
        assert!(
            in_root.iter().any(|(_, line)| line.contains(open)),
            "{} no longer calls {open}: the gate is checking for a call that has moved",
            root_module.display()
        );
    }
}
