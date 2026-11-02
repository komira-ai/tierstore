//! The `tierstore` binary: its flags and its start are in `tierstore::server::cli`; the stderr
//! subscriber it installs first is `tierstore::server::build_info::stderr_log`.

fn main() -> std::process::ExitCode {
    tierstore::server::build_info::stderr_log::install();
    tierstore::server::cli::main(std::env::args_os().collect())
}
