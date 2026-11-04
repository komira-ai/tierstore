//! The settings of the heartbeat of a proxy: `hot_store.heartbeat`, `hot_store.departure` and
//! `hot_store.deadline_margin`. They are in a struct of their own that
//! [`Cli`](super::cli::Cli) holds. The messages of the check name the old flags, which
//! [`keys_for_flags`](super::config::keys_for_flags) turns into the keys.

use std::time::Duration;

use super::config::{TierTable, duration};
use crate::ProxyConfig;

/// The default of `hot_store.heartbeat`.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// The default of `hot_store.departure`.
const DEPARTURE: Duration = Duration::from_secs(60);
/// The default of `hot_store.deadline_margin`.
const MARGIN: Duration = Duration::from_secs(10);

/// How the proxy heartbeats and when its owner counts as departed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatFlags {
    /// How often a proxy rewrites its registration in the hot tier, and the longest one such
    /// call may take. Longer than zero, and at most a sixth of `hot_store.departure`.
    pub heartbeat: Duration,
    /// How long a proxy that is not heard of stays live, less `hot_store.deadline_margin`: the value of
    /// `Proxy::is_live`. The puts and deletes made on entries read it, and
    /// the reconciler counts the owner of an entry as departed when its registration has stood
    /// unchanged this long. At least six times `hot_store.heartbeat`.
    pub departure: Duration,
    /// The time taken off `hot_store.departure` for `Proxy::is_live`, and the time that must be left
    /// before a call of the `Bounded` wrapper that destroys starts, which it is also cut at. The
    /// deletes made on entries send their calls through `Bounded`; the puts made on entries check
    /// the time themselves and cut their own cleanup call at it. Longer than zero, and at most a quarter of `hot_store.departure`.
    pub deadline_margin: Duration,
}

impl HeartbeatFlags {
    /// The values `[hot_store]` gives, and the default of each key it leaves out.
    pub(super) fn from_table(hot: &TierTable) -> Self {
        Self {
            heartbeat: duration(hot.heartbeat, HEARTBEAT),
            departure: duration(hot.departure, DEPARTURE),
            deadline_margin: duration(hot.deadline_margin, MARGIN),
        }
    }

    /// Refuses the values under which a process that is not heard of could still be taken for
    /// one that is, or the other way round: the one check of the start that guards the
    /// exclusion between a departed owner and whoever takes over from it. The other relations
    /// (the lease time against the renewal period, for one) are in the documentation.
    ///
    /// # Errors
    /// The text that names the values and the rule they break.
    pub(super) fn check(&self) -> Result<(), String> {
        let Self {
            heartbeat,
            departure,
            deadline_margin: margin,
        } = *self;
        if heartbeat.is_zero() {
            return Err("--heartbeat must be longer than zero".into());
        }
        if margin.is_zero() {
            return Err("--deadline-margin must be longer than zero".into());
        }
        if heartbeat.saturating_mul(6) > departure {
            return Err(format!(
                "--departure ({departure:?}) must be at least 6 times --heartbeat ({heartbeat:?})"
            ));
        }
        if margin.saturating_mul(4) > departure {
            return Err(format!(
                "--deadline-margin ({margin:?}) must be at most a quarter of --departure \
                 ({departure:?})"
            ));
        }
        Ok(())
    }

    /// Gives the values to the settings of the proxy.
    pub(super) fn apply(&self, proxy: &mut ProxyConfig) {
        proxy.heartbeat = self.heartbeat;
        proxy.departure = self.departure;
        proxy.deadline_margin = self.deadline_margin;
    }
}
