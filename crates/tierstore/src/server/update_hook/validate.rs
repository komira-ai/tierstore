//! The rules every value read from the record and the params is held to before any `nft`,
//! `systemctl` or link call: a record that fails one is not acted on, and the step exits 1 with
//! the reason `state_invalid`.

use super::declaration::Declaration;
use super::params::Params;
use super::record::{Marker, Record};

/// The rule a record or the params broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// `serving` or `previous` is not one of the two declared ports.
    #[error("{field} is {found}, not a declared port")]
    Port {
        /// The record field.
        field: &'static str,
        /// The record's value.
        found: u16,
    },
    /// `serving` and `previous` are the same port.
    #[error("serving and previous are the same port")]
    Same,
    /// A marker is set without the one it follows from.
    #[error("{mark} is set but {needs} is not")]
    Marks {
        /// The marker that is set.
        mark: &'static str,
        /// The one it needs.
        needs: &'static str,
    },
    /// Markers with no update in flight and no commit.
    #[error("markers are set with no update in flight")]
    Orphan,
    /// The staged release lies in the state directory.
    #[error("the staged release lies in the state directory")]
    Staged,
}

/// Runs every rule on `record`.
///
/// # Errors
/// The first [`Invalid`] rule the record breaks.
pub fn validate(record: &Record, declaration: &Declaration) -> Result<(), Invalid> {
    if !declaration.has(record.serving) {
        return Err(Invalid::Port {
            field: "serving",
            found: record.serving,
        });
    }
    if let Some(previous) = record.previous {
        if !declaration.has(previous) {
            return Err(Invalid::Port {
                field: "previous",
                found: previous,
            });
        }
        if previous == record.serving {
            return Err(Invalid::Same);
        }
    }
    let has = |m: Marker| record.markers.contains(&m);
    if has(Marker::Verified) && !has(Marker::Swapped) {
        return Err(Invalid::Marks {
            mark: "verified",
            needs: "swapped",
        });
    }
    if has(Marker::Stopping) && !has(Marker::Verified) {
        return Err(Invalid::Marks {
            mark: "stopping",
            needs: "verified",
        });
    }
    if record.previous.is_none() && !record.markers.is_empty() && !has(Marker::Stopping) {
        return Err(Invalid::Orphan);
    }
    Ok(())
}

/// Holds the params to the declaration: the staged release is not in the state directory.
///
/// # Errors
/// [`Invalid::Staged`].
pub fn check_params(params: &Params, declaration: &Declaration) -> Result<(), Invalid> {
    if params.staged_release.starts_with(&declaration.state_dir) {
        Err(Invalid::Staged)
    } else {
        Ok(())
    }
}
