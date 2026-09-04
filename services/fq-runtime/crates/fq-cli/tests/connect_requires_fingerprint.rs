//! `fq connect` with no pin and no terminal refuses — before it dials
//! anything, and so before the token can leave the process
//! (<https://github.com/bricef/factor-q/issues/544>).
//!
//! It used to pin whatever the network presented, print "non-interactive:
//! pinning automatically", and send the admin token to it. Every scripted
//! pairing was trust-on-first-use against an active attacker, on an edge
//! the daemon is allowed to bind non-loopback. Trust-on-first-use is now
//! interactive-only: a person is shown the fingerprint and asked; a script
//! passes `--fingerprint` from the file the daemon wrote beside its
//! identity, or gets an error that says exactly that.
//!
//! No daemon here. The address is a listener this test owns and never
//! accepts on, so "refused before dialling" is observable rather than
//! inferred from the wording of the error.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::{Command, Stdio};

#[test]
fn non_interactive_connect_without_a_pin_refuses_before_dialing() {
    // If `fq connect` probes the address before refusing, this
    // listener sees the connection.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
    listener.set_nonblocking(true).expect("non-blocking accept");
    let addr = listener.local_addr().expect("local addr").to_string();

    // A fresh XDG home: no stored pin for any address, and nothing the
    // developer's real connections.toml could contribute.
    let xdg = tempfile::tempdir().expect("xdg dir");

    let out = Command::new(env!("CARGO_BIN_EXE_fq"))
        .args(["connect", &addr, "--token", "not-a-real-token"])
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("HOME", xdg.path())
        .current_dir(xdg.path())
        // A pipe, not a terminal — what every script, CI job and test
        // has on stdin.
        .stdin(Stdio::piped())
        .output()
        .expect("run fq connect");

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unpinned, non-interactive connect must exit non-zero:\n{err}"
    );
    assert!(
        err.contains("--fingerprint"),
        "the refusal must name the flag that fixes it:\n{err}"
    );
    assert!(
        err.contains("edge/fingerprint"),
        "the refusal must say where the fingerprint lives:\n{err}"
    );
    assert!(
        err.contains("terminal"),
        "the refusal must say why there was no prompt:\n{err}"
    );
    assert!(
        !err.contains("pinning automatically"),
        "the old auto-pin notice must be gone:\n{err}"
    );

    match listener.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        Ok((_, peer)) => panic!("fq connect dialled {addr} from {peer} before refusing"),
        Err(e) => panic!("unexpected accept error: {e}"),
    }
    assert!(
        !xdg.path().join("factor-q").join("connections.toml").exists(),
        "a refused pairing must store nothing"
    );
}
