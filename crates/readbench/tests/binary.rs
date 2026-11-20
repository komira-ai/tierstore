//! The program itself: `main` forwards to the command line and exits with its code.

use std::process::Command;

#[test]
fn the_program_prints_its_help_and_refuses_what_is_not_a_command() {
    let program = env!("CARGO_BIN_EXE_readbench");
    let help = Command::new(program).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("Usage: readbench")
    );
    let bogus = Command::new(program).arg("bogus").output().unwrap();
    assert_eq!(bogus.status.code(), Some(2));
    assert!(
        String::from_utf8(bogus.stderr)
            .unwrap()
            .contains("unrecognized subcommand")
    );
}
