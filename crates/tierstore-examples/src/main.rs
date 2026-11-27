//! `tierstore-example`: runs an example of `examples/` against tierstore, or checks its page. See
//! the library docs; this file only forwards to [`tierstore_examples::cli::main`].

use std::process::ExitCode;

fn main() -> ExitCode {
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    ExitCode::from(tierstore_examples::cli::main(
        std::env::args_os(),
        &mut out,
        &mut err,
    ))
}
