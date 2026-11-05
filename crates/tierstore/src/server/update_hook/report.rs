//! The exit status and the JSON object a step writes as the last line of its standard output.
//!
//! ```json
//! {"class":"release","reason":"...","retry_after_secs":300,
//!  "reversible":true,"promotion_pending":false,"detail":"..."}
//! ```
//!
//! `class`, `reversible` and `promotion_pending` are always there; the others when set. The
//! updater reads the line leniently (a missing or malformed line is `{}`); this module writes it
//! in the shape the contract names, so the updater never has to guess. The phase is the
//! updater's to assign: the hook says only the class, and whether it can still undo.

use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{Map, Value};

use super::ports::{PortError, PortKind};
use super::record::{Invalid, RecordError};

/// The longest `reason` token.
pub const MAX_REASON_LEN: usize = 64;

/// Whether `reason` is a token the updater accepts: lowercase letters, digits and `_`, one to
/// [`MAX_REASON_LEN`] bytes.
#[must_use]
pub fn is_reason_token(reason: &str) -> bool {
    (1..=MAX_REASON_LEN).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Whom a failure is blamed on (it decides whether the fleet rolls back).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Class {
    /// The new version is wrong. The default.
    #[default]
    Release,
    /// The host or something shared failed; the release may be fine.
    Environment,
}

impl Class {
    /// The word in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Release => "release",
            Class::Environment => "environment",
        }
    }
}

/// The machine-readable reasons this hook gives. A closed set, so a token cannot be misspelt or
/// too long; a test checks every one against [`is_reason_token`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The command line was refused.
    BadArgs,
    /// The `--params` file was unreadable or wrong.
    BadParams,
    /// Another step holds the lock of the state directory.
    Locked,
    /// The record is not as the hook requires.
    StateInvalid,
    /// The step does not follow from the record: `cutover` before `prepare`, and so on.
    OutOfOrder,
    /// `cleanup` before a `verify` succeeded.
    NotVerified,
    /// `rollback` or `prepare` after `cleanup` began: nothing can be undone.
    Committed,
    /// `cleanup` could not confirm, freshly, that the new release serves: nothing was committed.
    NotConfirmed,
    /// The host cannot take the update now (exit 75): both instances run.
    HostNotReady,
    /// An instance that gave its sockets away is still draining (exit 75): try again soon.
    Draining,
    /// No instance answered `/readyz` with 200 on the public port.
    NotReady,
    /// The public port was never answered by the release with the version it must report.
    ProbeFailed,
    /// `verify`: the build gauge on the public port is not the target version.
    WrongBuild,
    /// `prepare`: the node's configuration has no `server.handoff_socket`, so the new instance
    /// could not take the old one's sockets.
    HandoffNotConfigured,
    /// `prepare`: there is no configuration file at the path `config_file` names.
    ConfigMissing,
    /// `prepare`: the staged release or the previous one refuses the node's configuration when
    /// it checks it (`--check-config`), so a start or a rollback onto it would fail.
    ConfigRefused,
    /// The params name a service manager this hook does not drive.
    ServiceManagerUnknown,
    /// `rollback` of a first install: there is no release to go back to; what `cutover`
    /// started is stopped.
    NoPreviousRelease,
    /// A unit or the launchd job could not be started, stopped or asked.
    UnitFailed,
    /// The record or a start marker could not be read or written.
    StateIo,
    /// The host could not be asked.
    HostFailed,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 21] = [
        Reason::BadArgs,
        Reason::BadParams,
        Reason::Locked,
        Reason::StateInvalid,
        Reason::OutOfOrder,
        Reason::NotVerified,
        Reason::Committed,
        Reason::NotConfirmed,
        Reason::HostNotReady,
        Reason::Draining,
        Reason::NotReady,
        Reason::ProbeFailed,
        Reason::WrongBuild,
        Reason::HandoffNotConfigured,
        Reason::ConfigMissing,
        Reason::ConfigRefused,
        Reason::ServiceManagerUnknown,
        Reason::NoPreviousRelease,
        Reason::UnitFailed,
        Reason::StateIo,
        Reason::HostFailed,
    ];

    /// The token in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::BadArgs => "bad_args",
            Reason::BadParams => "bad_params",
            Reason::Locked => "locked",
            Reason::StateInvalid => "state_invalid",
            Reason::OutOfOrder => "out_of_order",
            Reason::NotVerified => "not_verified",
            Reason::Committed => "committed",
            Reason::NotConfirmed => "not_confirmed",
            Reason::HostNotReady => "host_not_ready",
            Reason::Draining => "draining",
            Reason::NotReady => "not_ready",
            Reason::ProbeFailed => "probe_failed",
            Reason::WrongBuild => "wrong_build",
            Reason::HandoffNotConfigured => "handoff_not_configured",
            Reason::ConfigMissing => "config_missing",
            Reason::ConfigRefused => "config_refused",
            Reason::ServiceManagerUnknown => "service_manager_unknown",
            Reason::NoPreviousRelease => "no_previous_release",
            Reason::UnitFailed => "unit_failed",
            Reason::StateIo => "state_io",
            Reason::HostFailed => "host_failed",
        }
    }

    /// Whom the reason blames: the host, the node's configuration and the hook's own inputs are
    /// `environment`; what the new release (or its package) does or does not do is `release`.
    #[must_use]
    pub const fn class(self) -> Class {
        match self {
            Reason::BadArgs
            | Reason::BadParams
            | Reason::Locked
            | Reason::StateInvalid
            | Reason::HostNotReady
            | Reason::Draining
            | Reason::HandoffNotConfigured
            | Reason::ConfigMissing
            | Reason::ConfigRefused
            | Reason::UnitFailed
            | Reason::StateIo
            | Reason::HostFailed => Class::Environment,
            Reason::OutOfOrder
            | Reason::NotVerified
            | Reason::Committed
            | Reason::NotConfirmed
            | Reason::NotReady
            | Reason::ProbeFailed
            | Reason::WrongBuild
            | Reason::ServiceManagerUnknown
            | Reason::NoPreviousRelease => Class::Release,
        }
    }
}

/// What a step tells the updater besides its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Whom a failure is blamed on.
    pub class: Class,
    /// A short token; prose goes in `detail`.
    pub reason: Option<Reason>,
    /// With exit 75: when the updater may order the host again.
    pub retry_after_secs: Option<u64>,
    /// Whether the hook can still undo what it has done. `false` is the commit.
    pub reversible: bool,
    /// Set by `verify`: serving, but not yet fully its own.
    pub promotion_pending: bool,
    /// A short text for the log and the alert.
    pub detail: Option<String>,
}

impl Report {
    /// The line to print: one JSON object, no newline.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut object = Map::new();
        object.insert("class".to_owned(), Value::from(self.class.as_str()));
        object.insert("reversible".to_owned(), Value::from(self.reversible));
        object.insert(
            "promotion_pending".to_owned(),
            Value::from(self.promotion_pending),
        );
        let optional = [
            ("reason", self.reason.map(|r| Value::from(r.as_str()))),
            ("retry_after_secs", self.retry_after_secs.map(Value::from)),
            ("detail", self.detail.clone().map(Value::from)),
        ];
        for (key, value) in optional {
            object.extend(value.map(|value| (key.to_owned(), value)));
        }
        Value::Object(object).to_string()
    }

    /// Writes the line and a newline: what a step does last, to its standard output.
    ///
    /// # Errors
    /// The write failed.
    pub fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{}", self.to_line())
    }
}

/// How a step ended, as hostkeeper reads it. This hook never halts: a record it cannot trust is
/// exit 1, and hostkeeper decides what that means for its `rollback`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Success,
    /// Exit 1.
    Failure,
    /// Exit 75: not now; nothing irreversible was done by this run, try the step again later.
    Deferred,
}

impl Outcome {
    /// The status a step exits with.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Outcome::Success => 0,
            Outcome::Failure => 1,
            Outcome::Deferred => 75,
        }
    }
}

impl From<Outcome> for ExitCode {
    fn from(outcome: Outcome) -> ExitCode {
        ExitCode::from(outcome.code())
    }
}

/// How a step ended and what it tells the updater.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    /// The exit status.
    pub outcome: Outcome,
    /// The last line.
    pub report: Report,
}

impl Done {
    fn with(outcome: Outcome, reason: Option<Reason>, detail: String) -> Done {
        Done {
            outcome,
            report: Report {
                class: reason.map_or(Class::Release, Reason::class),
                reason,
                retry_after_secs: None,
                reversible: true,
                promotion_pending: false,
                detail: Some(detail),
            },
        }
    }

    /// Success, with a text for the log.
    #[must_use]
    pub fn ok(detail: impl Into<String>) -> Done {
        Done::with(Outcome::Success, None, detail.into())
    }

    /// Failure (exit 1) with a reason and a text; the class follows the reason.
    #[must_use]
    pub fn fail(reason: Reason, detail: impl Into<String>) -> Done {
        Done::with(Outcome::Failure, Some(reason), detail.into())
    }

    /// Deferred (exit 75): not now; hostkeeper runs the step again after `retry_after_secs`.
    #[must_use]
    pub fn deferred(reason: Reason, detail: impl Into<String>, retry_after_secs: u64) -> Done {
        let mut done = Done::with(Outcome::Deferred, Some(reason), detail.into());
        done.report.retry_after_secs = Some(retry_after_secs);
        done
    }
}

impl From<PortError> for Done {
    fn from(err: PortError) -> Done {
        let reason = match &err {
            PortError::Busy => Reason::Locked,
            PortError::Failed { kind, .. } => match kind {
                PortKind::Units | PortKind::Launchd => Reason::UnitFailed,
                PortKind::Markers | PortKind::State => Reason::StateIo,
                PortKind::Instances | PortKind::Host => Reason::HostFailed,
            },
        };
        Done::fail(reason, err.to_string())
    }
}

impl From<RecordError> for Done {
    fn from(err: RecordError) -> Done {
        Done::fail(Reason::StateInvalid, err.to_string())
    }
}

impl From<Invalid> for Done {
    fn from(err: Invalid) -> Done {
        Done::fail(Reason::StateInvalid, err.to_string())
    }
}
