//! The flags of the heartbeat of a proxy and of the removal of registrations that its owner left:
//! `--heartbeat`, `--departure`, `--deadline-margin` and `--registration-retention`, which the file
//! has as the keys of `[hot_store]` of the same names. They are in a struct of their own that
//! [`Cli`](super::cli::Cli) flattens, so that the flags of the process stay in one place and in one
//! file each.

use std::time::Duration;

use clap::Args;

use super::cli::parse_duration;
use super::config::{TierTable, duration};
use super::config_units::Dur;
use crate::{ProxyConfig, ReconcilerConfig};

/// The default of `--heartbeat`.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// The default of `--departure`.
const DEPARTURE: Duration = Duration::from_secs(60);
/// The default of `--deadline-margin`.
const MARGIN: Duration = Duration::from_secs(10);
/// The default of `--registration-retention`.
const RETENTION: Duration = Duration::from_secs(600);

/// How the proxy heartbeats and how long the reconciler waits.
#[derive(Clone, Debug, PartialEq, Eq, Args)]
pub struct HeartbeatFlags {
    /// How often a proxy rewrites its registration in the hot tier, and the longest one such
    /// call may take. Longer than zero, and at most a sixth of `--departure`.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub heartbeat: Duration,
    /// How long a registration may stay unchanged, as the reconciler sees it on its own clock,
    /// before its proxy counts as departed; also how long a proxy that is not heard of keeps its
    /// right to destroy and acknowledge, less `--deadline-margin`. At least six times
    /// `--heartbeat`.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    pub departure: Duration,
    /// The time that must be left of a proxy's right to destroy before it starts a call that
    /// destroys, and the longest such a call may take. Longer than zero, and at most a quarter of
    /// `--departure`.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub deadline_margin: Duration,
    /// How long the registration of a proxy that heartbeated must have stayed unchanged, as the
    /// reconciler sees it, before the reconciler removes it, if the proxy left no journal object.
    /// At least `--departure`.
    #[arg(long, default_value = "10m", value_parser = parse_duration)]
    pub registration_retention: Duration,
}

impl HeartbeatFlags {
    /// The values `[hot_store]` gives, and the default of each flag for a key it leaves out.
    pub(super) fn from_table(hot: &TierTable) -> Self {
        Self {
            heartbeat: duration(hot.heartbeat, HEARTBEAT),
            departure: duration(hot.departure, DEPARTURE),
            deadline_margin: duration(hot.deadline_margin, MARGIN),
            registration_retention: duration(hot.registration_retention, RETENTION),
        }
    }

    /// Writes the values as the keys of `[hot_store]`.
    pub(super) fn put_in(&self, hot: &mut TierTable) {
        hot.heartbeat = Some(Dur(self.heartbeat));
        hot.departure = Some(Dur(self.departure));
        hot.deadline_margin = Some(Dur(self.deadline_margin));
        hot.registration_retention = Some(Dur(self.registration_retention));
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
        if retention < departure {
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
