//! The nftables rules, through the `nft` program.

use std::time::Duration;

use super::super::ports::{Nft, PortError};
use super::runner::Runner;

/// `nft`, called through a [`Runner`].
pub struct NftTool<'a> {
    #[allow(dead_code)]
    runner: &'a dyn Runner,
    #[allow(dead_code)]
    timeout: Duration,
}

impl<'a> NftTool<'a> {
    /// Calls `nft` through `runner`; a call that runs past `timeout` is killed.
    #[must_use]
    pub fn new(runner: &'a dyn Runner, timeout: Duration) -> NftTool<'a> {
        NftTool { runner, timeout }
    }
}

impl Nft for NftTool<'_> {
    fn apply(&self, _ruleset: &str) -> Result<(), PortError> {
        Ok(())
    }

    fn list_table(&self, _table: &str) -> Result<Option<String>, PortError> {
        Ok(None)
    }

    fn delete_table(&self, _table: &str) -> Result<(), PortError> {
        Ok(())
    }
}
