//! `storebench`: forwards to the library's command line (`storebench::cli`).

fn main() -> std::process::ExitCode {
    storebench::cli::main()
}
