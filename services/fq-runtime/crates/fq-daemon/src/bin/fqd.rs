//! The `fqd` binary: the daemon, and nothing else.

fn main() -> std::process::ExitCode {
    fq_daemon::fqd_main()
}
