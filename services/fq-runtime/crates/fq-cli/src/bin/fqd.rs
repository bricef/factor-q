//! Thin binary shim — the daemon entry point. See `fq_cli::fqd_main`.
fn main() -> std::process::ExitCode {
    fq_cli::fqd_main()
}
