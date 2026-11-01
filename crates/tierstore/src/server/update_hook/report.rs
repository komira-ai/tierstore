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

use super::declaration::DeclarationError;
use super::ports::{PortError, PortKind};
use super::record::RecordError;
use super::validate::Invalid;

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
    /// The step exists on the command line and the machine's ports are not wired yet.
    NotImplemented,
    /// The command line was refused.
    BadArgs,
    /// The `--params` file was unreadable or wrong.
    BadParams,
    /// Another step holds the lock of the state directory.
    Locked,
    /// The declaration, the record or the params are not as the hook requires.
    StateInvalid,
    /// The step does not follow from the record: `cutover` before `prepare`, and so on.
    OutOfOrder,
    /// `cleanup` before a `verify` succeeded.
    NotVerified,
    /// `rollback`, `cutover` or `prepare` after `cleanup` began to stop v-current.
    Committed,
    /// The host cannot take the update now (exit 75): the guard, the conntrack bounds.
    HostNotReady,
    /// An instance did not answer `/readyz` with 200.
    NotReady,
    /// The new instance is read-write while the old one runs: two writers.
    UnexpectedMode,
    /// The public port was never answered by the new instance with the staged version.
    ProbeFailed,
    /// `verify`: the instance behind the public port is not the new one.
    WrongBuild,
    /// `verify`: the redirect does not name the new instance.
    RedirectWrong,
    /// `verify`: the old instance no longer answers.
    PreviousDown,
    /// The slot to start has no link.
    NoSlot,
    /// `nft` failed.
    NftFailed,
    /// A unit could not be started or stopped.
    UnitFailed,
    /// A slot link could not be read, made or removed.
    LinkFailed,
    /// The record could not be read or written.
    StateIo,
    /// The host could not be asked.
    HostFailed,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 21] = [
        Reason::NotImplemented,
        Reason::BadArgs,
        Reason::BadParams,
        Reason::Locked,
        Reason::StateInvalid,
        Reason::OutOfOrder,
        Reason::NotVerified,
        Reason::Committed,
        Reason::HostNotReady,
        Reason::NotReady,
        Reason::UnexpectedMode,
        Reason::ProbeFailed,
        Reason::WrongBuild,
        Reason::RedirectWrong,
        Reason::PreviousDown,
        Reason::NoSlot,
        Reason::NftFailed,
        Reason::UnitFailed,
        Reason::LinkFailed,
        Reason::StateIo,
        Reason::HostFailed,
    ];

    /// The token in the JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::NotImplemented => "not_implemented",
            Reason::BadArgs => "bad_args",
            Reason::BadParams => "bad_params",
            Reason::Locked => "locked",
            Reason::StateInvalid => "state_invalid",
            Reason::OutOfOrder => "out_of_order",
            Reason::NotVerified => "not_verified",
            Reason::Committed => "committed",
            Reason::HostNotReady => "host_not_ready",
            Reason::NotReady => "not_ready",
            Reason::UnexpectedMode => "unexpected_mode",
            Reason::ProbeFailed => "probe_failed",
            Reason::WrongBuild => "wrong_build",
            Reason::RedirectWrong => "redirect_wrong",
            Reason::PreviousDown => "previous_down",
            Reason::NoSlot => "no_slot",
            Reason::NftFailed => "nft_failed",
            Reason::UnitFailed => "unit_failed",
            Reason::LinkFailed => "link_failed",
            Reason::StateIo => "state_io",
            Reason::HostFailed => "host_failed",
        }
    }

    /// Whom the reason blames: the host and the hook's own inputs are `environment`; what the
    /// new release does (or does not do) is `release`.
    #[must_use]
    pub const fn class(self) -> Class {
        match self {
            Reason::BadArgs
            | Reason::BadParams
            | Reason::Locked
            | Reason::StateInvalid
            | Reason::HostNotReady
            | Reason::NoSlot
            | Reason::NftFailed
            | Reason::UnitFailed
            | Reason::LinkFailed
            | Reason::StateIo
            | Reason::HostFailed => Class::Environment,
            _ => Class::Release,
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

/// How a step ended, as the updater reads it. This hook never halts: a record it cannot trust is
/// exit 1, and the updater decides what that means for its `rollback`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Success,
    /// Exit 1.
    Failure,
    /// Exit 75: the host is not ready, nothing was done, try later.
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

    /// Deferred (exit 75): nothing was done; the updater may order the host again after
    /// `retry_after_secs`.
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
                PortKind::Nft => Reason::NftFailed,
                PortKind::Units => Reason::UnitFailed,
                PortKind::Slots => Reason::LinkFailed,
                PortKind::State => Reason::StateIo,
                PortKind::Instances | PortKind::Host => Reason::HostFailed,
            },
        };
        Done::fail(reason, err.to_string())
    }
}

impl From<DeclarationError> for Done {
    fn from(err: DeclarationError) -> Done {
        Done::fail(Reason::StateInvalid, err.to_string())
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
