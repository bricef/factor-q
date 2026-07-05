//! Query-style `fq` commands piped into a closed reader must die
//! silently on SIGPIPE like any Unix filter (`fq status | head`),
//! not panic with exit code 101. Rust's startup ignores SIGPIPE,
//! turning a closed pipe into an EPIPE write error that `println!`
//! panics on; `main` restores the default disposition for
//! non-daemon commands. Dogfood finding, 2026-07-05.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

/// Closing the pipe's read end before the child writes makes the
/// child's first stdout write raise SIGPIPE deterministically —
/// the kernel checks for readers at write time, not buffer space.
#[test]
fn query_command_dies_silently_when_stdout_closes() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fq"))
        .arg("version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fq version");
    drop(child.stdout.take());
    let status = child.wait().expect("wait for fq");
    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "expected silent SIGPIPE death, got {status:?} \
         (exit code 101 means the EPIPE panic is back)"
    );
}
