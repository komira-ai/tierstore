//! The `tierstore-index-measure` binary: forwards to [`tierstore_index::measure::main`].

fn main() -> std::process::ExitCode {
    tierstore_index::measure::main(std::env::args_os().skip(1))
}
