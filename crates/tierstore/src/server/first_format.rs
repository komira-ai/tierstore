//! The format of a hot tier that has none: the first start writes it.
//!
//! A hot tier with no `_tiering/format` object is new (the owner's rule, `docs/design/tierstore-design.md`
//! 13.1). A node that finds none, after it has registered and made its first beat, writes the
//! format its configuration names with a create-only put and goes on:
//!
//! - an `s3:` hot tier: `active-marks` at epoch 3, naming the marks bucket of the configuration;
//! - a `dir:` hot tier: `active` at epoch 2, as `format-epoch --activate` writes it;
//! - a `memory` hot tier: nothing here; [`start_format`](super::memory_format::start_format) wrote
//!   its format before the proxy opened.
//!
//! A format object that exists, of any phase, is never touched. Of two nodes that start at once
//! one wins the create-only put; the other reads the object and follows it. The answer of a put
//! that may or may not have been applied (`Error::Unavailable`) is not retried and is not an
//! error of the start: the node reads the object at once and at every beat, and follows it.
//! Nothing is listed and nothing is proven: the tier is new because it has no format object.

use std::time::Duration;

use super::cli::{Cli, Error, marks_name};
use crate::format::{ENTRIES_EPOCH, Format, FormatPhase, MARKS_EPOCH, format_body, read_format};
use crate::journal::FORMAT_KEY;
use crate::{Backend, Error as BackendError, Proxy};

/// The format the configuration of `cli` writes into a hot tier that has none, or none for a tier
/// whose format is not written here.
fn target(cli: &Cli) -> Option<Format> {
    if cli.hot.starts_with("s3:") {
        return Some(Format {
            phase: FormatPhase::ActiveMarks,
            epoch: Some(MARKS_EPOCH),
            marks_bucket: marks_name(&cli.hot_marks),
        });
    }
    cli.hot.starts_with("dir:").then_some(Format {
        phase: FormatPhase::Active,
        epoch: Some(ENTRIES_EPOCH),
        marks_bucket: None,
    })
}

/// Writes the format of `cli` into `hot` if `hot` has no format object, and then reads the object
/// into `proxy`, within `limit`.
///
/// # Errors
/// The text of the hot tier's error if the create-only put fails with an answer that is neither
/// "the object exists" nor "unavailable".
pub(super) async fn first_format(
    cli: &Cli,
    proxy: &Proxy,
    hot: &dyn Backend,
    limit: Duration,
) -> Result<(), Error> {
    let Some(format) = target(cli) else {
        return Ok(());
    };
    // Any answer but "not there" (a format of any phase, or a read that fails) leaves the object
    // to the operator and the beats.
    if matches!(read_format(hot).await, Ok(None)) {
        match hot.put(FORMAT_KEY, format_body(&format)).await {
            Ok(()) | Err(BackendError::AlreadyExists(_)) => {}
            Err(BackendError::Unavailable(why)) => {
                tracing::warn!(
                    "tierstore: the answer to writing the format of the new hot tier is not \
                     known ({why}); reading it at each beat"
                );
            }
            Err(why) => {
                return Err(format!("writing the format of the new hot tier: {why}").into());
            }
        }
    }
    proxy.read_format_now(limit).await;
    Ok(())
}

#[cfg(test)]
#[path = "first_format_tests.rs"]
mod tests;
