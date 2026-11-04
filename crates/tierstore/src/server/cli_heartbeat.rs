//! The settings of the heartbeat of a proxy and of the removal of registrations that its owner left:
//! `hot_store.heartbeat`, `hot_store.departure`, `hot_store.deadline_margin` and
//! `hot_store.registration_retention`. They are in a struct of their own that
//! [`Cli`](super::cli::Cli) holds. The messages of the check name the old flags, which
//! [`keys_for_flags`](super::config::keys_for_flags) turns into the keys.

use std::time::Duration;

use super::config::{TierTable, duration};
use crate::{ProxyConfig, ReconcilerConfig};

/// The default of `hot_store.heartbeat`.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// The default of `hot_store.departure`.
const DEPARTURE: Duration = Duration::from_secs(60);
/// The default of `hot_store.deadline_margin`.
const MARGIN: Duration = Duration::from_secs(10);
/// The default of `hot_store.registration_retention`.
const RETENTION: Duration = Duration::ZERO;

/// How the proxy heartbeats and how long the reconciler waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatFlags {
    /// How often a proxy rewrites its registration in the hot tier, and the longest one such
    /// call may take. Longer than zero, and at most a sixth of `hot_store.departure`.
    pub heartbeat: Duration,
    /// How long a proxy that is not heard of stays live, less `hot_store.deadline_margin`: the value of
    /// `Proxy::is_live`. The puts and deletes made on entries read it; the
    /// removal of registrations uses `hot_store.registration_retention`. At least six times
    /// `hot_store.heartbeat`.
    pub departure: Duration,
    /// The time taken off `hot_store.departure` for `Proxy::is_live`, and the time that must be left
    /// before a call of the `Bounded` wrapper that destroys starts, which it is also cut at. The
    /// deletes made on entries send their calls through `Bounded`; the puts made on entries check
    /// the time themselves and cut their own cleanup call at it. Longer than zero, and at most a quarter of `hot_store.departure`.
    pub deadline_margin: Duration,
    /// How long the registration of a proxy that heartbeated must have stayed unchanged, as the
    /// reconciler sees it, before the reconciler removes it, if the proxy left no journal object.
    /// At least `hot_store.departure`, or zero, which is the default and removes no registration: a proxy
    /// of a release without the heartbeat that reopens a registration written by this release
    /// never rewrites it, so set a time only once no such proxy runs.
    pub registration_retention: Duration,
}

impl HeartbeatFlags {
    /// The values `[hot_store]` gives, and the default of each key it leaves out.
    pub(super) fn from_table(hot: &TierTable) -> Self {
        Self {
            heartbeat: duration(hot.heartbeat, HEARTBEAT),
            departure: duration(hot.departure, DEPARTURE),
            deadline_margin: duration(hot.deadline_margin, MARGIN),
            registration_retention: duration(hot.registration_retention, RETENTION),
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
            registration_retention: retention,
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
        if !retention.is_zero() && retention < departure {
            return Err(format!(
                "--registration-retention ({retention:?}) must be at least --departure \
                 ({departure:?})"
            ));
        }
        Ok(())
    }

    /// Gives the values to the settings of the proxy and of the reconciler.
    pub(super) fn apply(&self, proxy: &mut ProxyConfig, reconciler: &mut ReconcilerConfig) {
        proxy.heartbeat = self.heartbeat;
        proxy.departure = self.departure;
        proxy.deadline_margin = self.deadline_margin;
        reconciler.registration_retention = self.registration_retention;
    }
}
