//! `objstore-conformance`: the suite against an S3 endpoint named by flags (see `cli`).

fn main() -> std::process::ExitCode {
    objstore_conformance::cli::main(std::env::args_os())
}
