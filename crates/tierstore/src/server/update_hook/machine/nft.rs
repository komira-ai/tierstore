//! The nftables rules, through the `nft` program.

use std::time::Duration;

use super::super::ports::{Nft, PortError, PortKind};
use super::runner::{Call, Output, Runner, checked, run};

const PROGRAM: &str = "nft";
const KIND: PortKind = PortKind::Nft;
/// What `nft` says on standard error when the table it was asked about is not there
/// (`Error: No such file or directory`, the text of ENOENT).
const ABSENT: &str = "No such file or directory";

/// `nft`, called through a [`Runner`].
pub struct NftTool<'a> {
    runner: &'a dyn Runner,
    timeout: Duration,
}

impl<'a> NftTool<'a> {
    /// Calls `nft` through `runner`; a call that runs past `timeout` is killed.
    #[must_use]
    pub fn new(runner: &'a dyn Runner, timeout: Duration) -> NftTool<'a> {
        NftTool { runner, timeout }
    }

    fn call(&self, args: &[&str]) -> Call {
        Call::new(PROGRAM, args, self.timeout)
    }
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
        let call = self.call(&["list", "table", "inet", table]);
        let output = run(self.runner, &call, KIND)?;
        if is_absent(&output) {
            return Ok(None);
        }
        checked(output, &call, KIND)
            .map(|output| Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    }

    fn delete_table(&self, table: &str) -> Result<(), PortError> {
        let call = self.call(&["delete", "table", "inet", table]);
        let output = run(self.runner, &call, KIND)?;
        if is_absent(&output) {
            return Ok(());
        }
        checked(output, &call, KIND).map(|_| ())
    }
}
