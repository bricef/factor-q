//! Thin binary shim — everything lives in the fq-cli library.
fn main() -> std::process::ExitCode {
    fq_cli::fq_main()
}
