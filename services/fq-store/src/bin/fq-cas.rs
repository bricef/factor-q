//! The `fq-cas` binary — a thin wrapper over [`fq_store::cli`].

fn main() -> std::process::ExitCode {
    fq_store::cli::main()
}
