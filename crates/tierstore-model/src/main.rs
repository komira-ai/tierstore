//! Forwards to the library: `cargo run -p tierstore-model -- [all|heavy|<scenario id>...] [-v]`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match tierstore_model::cli::run_cli(&args, &mut std::io::stdout()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) | Err(_) => ExitCode::FAILURE,
    }
}
