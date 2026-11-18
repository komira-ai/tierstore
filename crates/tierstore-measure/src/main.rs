//! `ts-measure`: forwards to the library (`tierstore_measure::cli`).

use std::process::ExitCode;

use tierstore_measure::cli::{Env, main_with};
use tierstore_measure::clock::RealClock;
use tierstore_measure::run::System;

fn main() -> ExitCode {
    let clock = RealClock::new();
    let env = Env {
        runner: &System,
        clock: &clock,
    };
    ExitCode::from(main_with(
        std::env::args_os(),
        &env,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
