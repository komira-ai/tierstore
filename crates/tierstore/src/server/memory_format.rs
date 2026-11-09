//! The format of a hot tier that is made at the start.
//!
//! A node over a hot tier of the kind `memory` starts the tier empty: no release before this one
//! wrote to it, so there is nothing to drain and nothing else could ever write its format, which
//! is gone with the process. It starts at the active format, `{"epoch": 2, "phase": "active"}`, so
//! that the node accepts writes. A hot tier of any other kind keeps what is in it across starts;
//! one that has no format object is given it after the first beat (`first_format.rs`).

use super::cli::{Cli, Error};
use crate::Backend;
use crate::format::{ENTRIES_EPOCH, Format, FormatPhase, format_body};
use crate::journal::FORMAT_KEY;

/// Writes the active format into `hot` if the hot tier of `cli` is a `memory` tier.
///
/// # Errors
/// The text of the hot tier's error if the write fails.
pub(super) async fn start_format(cli: &Cli, hot: &dyn Backend) -> Result<(), Error> {
    if cli.hot != "memory" {
        return Ok(());
    }
    let active = Format {
        phase: FormatPhase::Active,
        epoch: Some(ENTRIES_EPOCH),
        marks_bucket: None,
    };
    hot.put(FORMAT_KEY, format_body(&active))
        .await
        .map_err(|why| format!("writing the format of the memory hot tier: {why}").into())
}

#[cfg(test)]
#[path = "memory_format_tests.rs"]
mod tests;
