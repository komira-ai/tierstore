//! What the heartbeat tells the operator: a beat that failed (once for each distinct cause, until a
//! beat is acknowledged), and the transitions of the registration, when it lapses and when it is
//! live again. A beat that is acknowledged, and a failure that repeats, say nothing.
//!
//! The lines are made here from values, with no clock of their own: the loop gives the times of
//! its monotonic clock, so a test asserts a line whole.

use crate::Error;

/// A line for the operator, and whether it is a warning (the other kind is information).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Said {
    pub(super) warn: bool,
    pub(super) text: String,
}

/// What the loop has already said about its beats.
pub(super) struct BeatLog {
    /// Where the beats go, as the operator named it.
    endpoint: String,
    /// The cause of the failure that was said last, none after a beat that was acknowledged.
    cause: Option<String>,
    /// Whether the registration was live after the last beat.
    live: bool,
    /// The send time of the last beat that was acknowledged, in milliseconds of the monotonic
    /// clock.
    last_ack: u64,
    /// The send time of the last acknowledged beat before the registration lapsed, while it is
    /// lapsed and the lapse has been said.
    lapsed_from: Option<u64>,
}

impl BeatLog {
    pub(super) fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            cause: None,
            live: false,
            last_ack: 0,
            lapsed_from: None,
        }
    }

    /// Takes the end of a beat sent at `started` and ended at `ended` (milliseconds of the
    /// monotonic clock), and whether the registration is live now; gives what is to be said.
    pub(super) fn after(
        &mut self,
        started: u64,
        ended: u64,
        result: &Result<(), Error>,
        live: bool,
    ) -> Vec<Said> {
        let mut said = Vec::new();
        match result {
            Ok(()) => {
                self.cause = None;
                self.last_ack = started;
            }
            Err(error) => {
                let cause = error.to_string();
                if self.cause.as_deref() != Some(cause.as_str()) {
                    said.push(Said {
                        warn: true,
                        text: format!(
                            "tierstore: heartbeat: the beat to {} failed after {} ms: {cause}",
                            self.endpoint,
                            ended.saturating_sub(started),
                        ),
                    });
                    self.cause = Some(cause);
                }
            }
        }
        if self.live && !live {
            self.lapsed_from = Some(self.last_ack);
            said.push(Said {
                warn: true,
                text: format!(
                    "tierstore: heartbeat: the registration has lapsed: no beat to {} has been \
                     acknowledged for {} ms, and this proxy refuses writes until one is",
                    self.endpoint,
                    ended.saturating_sub(self.last_ack),
                ),
            });
        } else if let (false, true, Some(from)) = (self.live, live, self.lapsed_from) {
            self.lapsed_from = None;
            said.push(Said {
                warn: false,
                text: format!(
                    "tierstore: heartbeat: the registration is live again: a beat to {} was \
                     acknowledged after {} ms without one",
                    self.endpoint,
                    started.saturating_sub(from),
                ),
            });
        }
        self.live = live;
        said
    }
}

/// Writes what was said to the operator's log.
#[cfg(feature = "server")]
pub(super) fn say(said: &[Said]) {
    for line in said {
        if line.warn {
            tracing::warn!("{}", line.text);
        } else {
            tracing::info!("{}", line.text);
        }
    }
}

/// Without the server there is no log to write to.
#[cfg(not(feature = "server"))]
pub(super) fn say(said: &[Said]) {
    let _ = said;
}
