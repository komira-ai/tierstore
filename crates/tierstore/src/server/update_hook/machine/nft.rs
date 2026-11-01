//! The nftables rules, through the `nft` program.

use std::time::Duration;

use super::super::ports::{Nft, PortError, PortKind};
use super::runner::{Call, Output, Runner, checked, run};

/// Where the Debian and Ubuntu package puts `nft`.
pub const DEFAULT_PROGRAM: &str = "/usr/sbin/nft";
const KIND: PortKind = PortKind::Nft;
/// What `nft` says on standard error when the table it was asked about is not there
/// (`Error: No such file or directory`, the text of ENOENT).
const ABSENT: &str = "No such file or directory";

/// `nft`, called through a [`Runner`].
pub struct NftTool<'a> {
    runner: &'a dyn Runner,
    timeout: Duration,
    program: String,
}

impl<'a> NftTool<'a> {
    /// Calls `nft` through `runner`; a call that runs past `timeout` is killed.
    #[must_use]
    pub fn new(runner: &'a dyn Runner, timeout: Duration) -> NftTool<'a> {
        NftTool {
            runner,
            timeout,
            program: DEFAULT_PROGRAM.to_owned(),
        }
    }

    /// The same, calling `program` (an absolute path) instead of the default.
    #[must_use]
    pub fn with_program(mut self, program: &str) -> NftTool<'a> {
        program.clone_into(&mut self.program);
        self
    }

    fn call(&self, args: &[&str]) -> Call {
        Call::new(&self.program, args, self.timeout)
    }
}

/// Whether `table` is a name the port will pass on: 1 to 64 characters, letters, digits and
/// underscores, not starting with a digit. Anything else is refused before a program runs.
fn check_table(table: &str) -> Result<(), PortError> {
    let name = table.len() <= 64
        && table
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name {
        return Ok(());
    }
    Err(PortError::failed(
        KIND,
        format!("{table:?} is not a table name"),
    ))
}

/// Whether `output` is `nft` saying that the table is not there: a non-zero exit with ENOENT's
/// text on standard error. Any other failure is a failure.
fn is_absent(output: &Output) -> bool {
    output.code != Some(0) && output.stderr_text().contains(ABSENT)
}

impl Nft for NftTool<'_> {
    fn apply(&self, ruleset: &str) -> Result<(), PortError> {
        let call = self.call(&["-f", "-"]).with_stdin(ruleset.as_bytes());
        checked(run(self.runner, &call, KIND)?, &call, KIND).map(|_| ())
    }

    fn list_table(&self, table: &str) -> Result<Option<String>, PortError> {
        check_table(table)?;
        let call = self.call(&["list", "table", "inet", table]);
        let output = run(self.runner, &call, KIND)?;
        if is_absent(&output) {
            return Ok(None);
        }
        checked(output, &call, KIND)
            .map(|output| Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    }

    fn delete_table(&self, table: &str) -> Result<(), PortError> {
        check_table(table)?;
        let call = self.call(&["delete", "table", "inet", table]);
        let output = run(self.runner, &call, KIND)?;
        if is_absent(&output) {
            return Ok(());
        }
        checked(output, &call, KIND).map(|_| ())
    }
}
