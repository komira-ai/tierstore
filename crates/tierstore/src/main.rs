//! The `tierstore` binary: its flags and its start are in `tierstore::server::cli`, which also
//! installs the stderr subscriber.

fn main() -> std::process::ExitCode {
    tierstore::server::cli::main(std::env::args_os().collect())
}
