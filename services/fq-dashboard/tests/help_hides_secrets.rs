//! `fq-dashboard --help` must never render the token's value.
//!
//! clap prints `[env: FQ_EDGE_TOKEN=<value>]` in the help text for any
//! `env`-backed argument whose variable is set, unless the argument
//! opts out with `hide_env_values`. The dashboard's token is set in the
//! environment by design (`dashboard.sh`), so without the opt-out a
//! routine `--help` on the host — or in a bug report — would print the
//! capability token (<https://github.com/bricef/factor-q/issues/545>).
//! The store CLI got this right for `FQ_BISCUIT_PRIVATE_KEY`; this test
//! keeps the dashboard honest the same way, against the real binary.

use std::process::Command;

#[test]
fn help_never_renders_the_edge_token() {
    let token = "hunter2-this-string-must-not-appear-in-help";
    let out = Command::new(env!("CARGO_BIN_EXE_fq-dashboard"))
        .arg("--help")
        .env("FQ_EDGE_TOKEN", token)
        .output()
        .expect("run fq-dashboard --help");
    assert!(out.status.success(), "--help must exit 0: {out:?}");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("FQ_EDGE_TOKEN"),
        "the help still names the variable so an operator can find it:\n{help}"
    );
    assert!(
        !help.contains(token),
        "the help must not render the token's value:\n{help}"
    );
}
