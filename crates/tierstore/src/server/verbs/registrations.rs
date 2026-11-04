//! The registrations of the proxies, `_tiering/proxies/<id>`, as the operator's verbs classify
//! them: which ids are running, which are not, and which cannot be told.
//!
//! A registration of a release with a heartbeat has a JSON body with the protocol number of the
//! process, `{"proto": 1, ...}`, and is written again on every beat, so the time of its last write
//! says whether the process is alive. A registration of an earlier release is an empty object: it
//! says nothing of whether its process is alive, and only the operator, who looks at the machines,
//! can say that it is not. The verbs refuse to pause writes while a registration that is not
//! running is not named by the operator, since an old writer that goes on writing during the pause
//! is what the pause is meant to rule out.

use std::time::Duration;

use serde::Deserialize;

use super::VerbError;
use crate::journal::PROXIES_PREFIX;
use crate::{Backend, Error};

/// What a registration says of its process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    /// The body has a protocol number and was written within the window of fresh beats.
    Live { proto: u32 },
    /// The body has a protocol number and was not written within the window.
    Silent { proto: u32 },
    /// The body has no protocol number: a release before the heartbeat wrote it, and nothing of
    /// it says whether the process is alive.
    Unlabelled,
}

/// One registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Registration {
    /// The id of the proxy.
    pub(super) id: String,
    /// What its body and its age say.
    pub(super) class: Class,
    /// Milliseconds since the object was last written.
    pub(super) age_ms: u64,
}

impl Registration {
    /// One line for the operator.
    pub(super) fn describe(&self) -> String {
        let seconds = self.age_ms / 1000;
        match self.class {
            Class::Live { proto } => format!("live, protocol {proto}, written {seconds}s ago"),
            Class::Silent { proto } => {
                format!("silent, protocol {proto}, written {seconds}s ago")
            }
            Class::Unlabelled => format!("unlabelled, no heartbeat, written {seconds}s ago"),
        }
    }
}

/// The part of a registration body that the verbs read; other fields are ignored.
#[derive(Deserialize)]
struct Beat {
    proto: Option<u32>,
}

/// Every registration in the hot tier, in the order of the ids, classified at the time `now`
/// (milliseconds, the clock of the hot tier's times): a body with a protocol number is live when
/// it was written less than `fresh` ago.
///
/// # Errors
/// What the hot tier gives. A registration deleted between the listing and its read is left out.
pub(super) async fn read(
    hot: &dyn Backend,
    now: u64,
    fresh: Duration,
) -> Result<Vec<Registration>, Error> {
    let fresh_ms = u64::try_from(fresh.as_millis()).unwrap_or(u64::MAX);
    let mut found = Vec::new();
    for object in hot.list(PROXIES_PREFIX).await? {
        let body = match hot.get(&object.key, None).await {
            Ok(body) => body,
            Err(Error::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        let age_ms = now.saturating_sub(object.modified);
        let proto = serde_json::from_slice::<Beat>(&body)
            .ok()
            .and_then(|beat| beat.proto);
        let class = match proto {
            Some(proto) if age_ms < fresh_ms => Class::Live { proto },
            Some(proto) => Class::Silent { proto },
            None => Class::Unlabelled,
        };
        let id = object
            .key
            .strip_prefix(PROXIES_PREFIX)
            .unwrap_or(&object.key);
        found.push(Registration {
            id: id.to_owned(),
            class,
            age_ms,
        });
    }
    Ok(found)
}

/// Whether every registration is running or named by the operator as departed, for the verb
/// `verb` (`drain` or `activate`).
///
/// # Errors
/// [`VerbError::Refused`]: an id of `departed` has no registration, or has a live one; or
/// registrations that are not live are not named.
pub(super) fn check_departed(
    registrations: &[Registration],
    departed: &[String],
    verb: &str,
) -> Result<(), VerbError> {
    for id in departed {
        let named = registrations.iter().find(|found| &found.id == id);
        match named.map(|found| found.class) {
            None => {
                return Err(VerbError::Refused(format!(
                    "--departed {id}: there is no registration of that id"
                )));
            }
            Some(Class::Live { .. }) => {
                return Err(VerbError::Refused(format!(
                    "--departed {id}: it has a fresh heartbeat, so it is running"
                )));
            }
            Some(_) => {}
        }
    }
    let unnamed: Vec<&str> = registrations
        .iter()
        .filter(|found| !matches!(found.class, Class::Live { .. }))
        .filter(|found| !departed.contains(&found.id))
        .map(|found| found.id.as_str())
        .collect();
    if unnamed.is_empty() {
        return Ok(());
    }
    Err(VerbError::Refused(format!(
        "refusing to {verb}: registrations with no fresh heartbeat that --departed does not \
         name: {}",
        unnamed.join(", ")
    )))
}

/// Whether every live registration has a protocol number of at least `minimum`. A registration
/// the operator named as departed is not live (see [`check_departed`]), so none is excused.
///
/// # Errors
/// [`VerbError::Refused`] naming the registrations below it.
pub(super) fn check_protocol(
    registrations: &[Registration],
    minimum: u32,
) -> Result<(), VerbError> {
    let behind: Vec<&str> = registrations
        .iter()
        .filter(|found| matches!(found.class, Class::Live { proto } if proto < minimum))
        .map(|found| found.id.as_str())
        .collect();
    if behind.is_empty() {
        return Ok(());
    }
    Err(VerbError::Refused(format!(
        "refusing to activate: registrations below protocol {minimum}: {}",
        behind.join(", ")
    )))
}

#[cfg(test)]
#[path = "registrations_tests.rs"]
mod tests;
