//! The launchd jobs over a recording runner: the exact `launchctl` calls, the wait for the
//! domain to let a job go, and every way a call can fail.

use std::cell::Cell;
use std::path::Path;
use std::time::Duration;

use super::super::ports::{Clock, Launchd, PortError, PortKind};
use super::testing::{FakeRunner, exit, signalled};
use super::{
    Call, LAUNCHCTL_NOT_FOUND, LAUNCHCTL_PROGRAM, LaunchctlJobs, LaunchdTimes, RunError,
    bootout_args, bootstrap_args, print_args,
};

const CALL: Duration = Duration::from_secs(11);
const BOOTOUT: Duration = Duration::from_secs(13);
const POLL: Duration = Duration::from_millis(17);
const GONE: u32 = 3;
const LABEL: &str = "com.example.tierstore";
const PLIST: &str = "/Library/LaunchDaemons/tierstore.plist";
const TARGET: &str = "system/com.example.tierstore";

/// A clock that counts the pauses it is asked for and waits for nothing.
#[derive(Default)]
struct Pauses {
    count: Cell<u32>,
    last: Cell<Duration>,
}

impl Clock for Pauses {
    fn now_ms(&self) -> u64 {
        0
    }

    fn pause(&self, span: Duration) {
        self.count.set(self.count.get() + 1);
        self.last.set(span);
    }
}

fn times() -> LaunchdTimes {
    LaunchdTimes {
        call: CALL,
        bootout: BOOTOUT,
        gone_attempts: GONE,
        poll: POLL,
    }
}

fn jobs<'a>(runner: &'a FakeRunner, clock: &'a Pauses) -> LaunchctlJobs<'a> {
    LaunchctlJobs::new(runner, clock, times())
}

fn loaded() -> Result<super::Output, RunError> {
    exit(
        0,
        "system/com.example.tierstore = {\n\tstate = running\n}\n",
        "",
    )
}

fn absent() -> Result<super::Output, RunError> {
    exit(
        LAUNCHCTL_NOT_FOUND,
        "",
        "Could not find service \"com.example.tierstore\" in domain for system",
    )
}

fn ok() -> Result<super::Output, RunError> {
    exit(0, "", "")
}

fn call(args: &[&str], timeout: Duration) -> Call {
    Call::new(LAUNCHCTL_PROGRAM, args, timeout)
}

fn launchd_message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Launchd,
            message,
        } => message,
        other => panic!("not a launchd failure: {other:?}"),
    }
}

/// Asserts the argument vectors of the three calls and the program: `/bin/launchctl`, the
/// `system` domain.
#[test]
fn the_argument_vectors_are_launchds() {
    assert_eq!(LAUNCHCTL_PROGRAM, "/bin/launchctl");
    assert_eq!(print_args(LABEL), ["print", TARGET]);
    assert_eq!(bootout_args(LABEL), ["bootout", TARGET]);
    assert_eq!(bootstrap_args(PLIST), ["bootstrap", "system", PLIST]);
}

/// Asserts that `loaded` is `print system/<label>` with the call's time limit, true for exit 0
/// and false for 113.
#[test]
fn loaded_asks_the_domain_with_print() {
    let runner = FakeRunner::replying(vec![loaded(), absent()]);
    let clock = Pauses::default();
    assert!(jobs(&runner, &clock).loaded(LABEL).unwrap());
    assert!(!jobs(&runner, &clock).loaded(LABEL).unwrap());
    assert_eq!(
        runner.calls(),
        [
            call(&["print", TARGET], CALL),
            call(&["print", TARGET], CALL)
        ]
    );
}

/// Asserts that `bootstrap` is `bootstrap system <plist>` with the call's time limit.
#[test]
fn bootstrap_loads_the_plist_into_the_system_domain() {
    let runner = FakeRunner::replying(vec![ok()]);
    let clock = Pauses::default();
    jobs(&runner, &clock).bootstrap(Path::new(PLIST)).unwrap();
    assert_eq!(
        runner.calls(),
        [call(&["bootstrap", "system", PLIST], CALL)]
    );
}

/// Asserts that a failed `bootstrap` is an error with the status and the program's words.
#[test]
fn a_failed_bootstrap_is_an_error() {
    let runner = FakeRunner::replying(vec![exit(37, "", "Operation already in progress")]);
    let clock = Pauses::default();
    let error = jobs(&runner, &clock)
        .bootstrap(Path::new(PLIST))
        .unwrap_err();
    assert!(launchd_message(error).ends_with("exited 37: Operation already in progress"),);
}

/// Asserts that `bootout` of a loaded job is `print`, `bootout system/<label>` with the longer
/// time limit of a drain, and then `print` until the domain no longer has the job, one pause of
/// the poll between two looks.
#[test]
fn bootout_removes_the_job_and_waits_until_the_domain_lets_go() {
    let runner = FakeRunner::replying(vec![loaded(), ok(), loaded(), absent()]);
    let clock = Pauses::default();
    jobs(&runner, &clock).bootout(LABEL).unwrap();
    assert_eq!(
        runner.calls(),
        [
            call(&["print", TARGET], CALL),
            call(&["bootout", TARGET], BOOTOUT),
            call(&["print", TARGET], CALL),
            call(&["print", TARGET], CALL),
        ]
    );
    assert_eq!(clock.count.get(), 1);
    assert_eq!(clock.last.get(), POLL);
}

/// Asserts that `bootout` of a job the domain does not have is one `print` and nothing else.
#[test]
fn bootout_of_a_job_that_is_not_loaded_does_nothing() {
    let runner = FakeRunner::replying(vec![absent()]);
    let clock = Pauses::default();
    jobs(&runner, &clock).bootout(LABEL).unwrap();
    assert_eq!(runner.calls(), [call(&["print", TARGET], CALL)]);
}

/// Asserts that a `bootout` that exits non-zero is an error, with the program, the status and
/// its standard error in the message, whether or not the job is gone afterwards: no further
/// call is made, so a failure is never taken for success.
#[test]
fn a_bootout_that_launchctl_refuses_is_an_error() {
    let runner = FakeRunner::replying(vec![
        loaded(),
        exit(5, "", "Boot-out failed: 5: Input/output error"),
    ]);
    let clock = Pauses::default();
    let error = jobs(&runner, &clock).bootout(LABEL).unwrap_err();
    assert_eq!(
        launchd_message(error),
        "/bin/launchctl bootout system/com.example.tierstore: exited 5: Boot-out failed: 5: Input/output error"
    );
    assert_eq!(runner.calls().len(), 2);
    assert_eq!(clock.count.get(), 0);
}

/// Asserts that a job that is still loaded after a `bootout` that succeeded, through every look,
/// fails with a message that says so, after exactly the configured number of looks.
#[test]
fn a_job_that_stays_loaded_fails_the_bootout() {
    let mut replies = vec![loaded(), ok()];
    replies.extend((0..GONE).map(|_| loaded()));
    let runner = FakeRunner::replying(replies);
    let clock = Pauses::default();
    let error = jobs(&runner, &clock).bootout(LABEL).unwrap_err();
    assert_eq!(
        launchd_message(error),
        "system/com.example.tierstore is still loaded after bootout and 3 looks"
    );
    assert_eq!(runner.calls().len(), 2 + GONE as usize);
    assert_eq!(clock.count.get(), GONE);
}

/// Asserts that a `print` that neither finds nor misses the job (any status but 0 and 113, a
/// signal, a program that cannot run or that runs past its time) fails `loaded` and `bootout`
/// before anything else is called.
#[test]
fn every_other_answer_of_print_is_a_failure() {
    let clock = Pauses::default();
    for reply in [
        exit(1, "", "Unrecognized target specifier."),
        signalled(),
        Err(RunError::Spawn("No such file or directory".to_owned())),
        Err(RunError::Timeout(CALL)),
    ] {
        for bootout in [false, true] {
            let runner = FakeRunner::replying(vec![reply.clone()]);
            let jobs = jobs(&runner, &clock);
            let message = launchd_message(if bootout {
                jobs.bootout(LABEL).unwrap_err()
            } else {
                jobs.loaded(LABEL).map(|_| ()).unwrap_err()
            });
            assert!(
                message.starts_with("/bin/launchctl print system/com.example.tierstore: "),
                "{message}"
            );
            assert_eq!(runner.calls().len(), 1);
        }
    }
}

/// Asserts that a look during the wait that fails is an error of the bootout and not "gone".
#[test]
fn a_failed_look_while_waiting_is_an_error() {
    let runner = FakeRunner::replying(vec![loaded(), ok(), exit(1, "", "odd")]);
    let clock = Pauses::default();
    assert!(jobs(&runner, &clock).bootout(LABEL).is_err());
}

/// Asserts that a label that is no label is refused before a program runs, by `loaded` and by
/// `bootout`.
#[test]
fn a_label_that_is_not_a_label_is_refused_before_any_call() {
    let runner = FakeRunner::replying(vec![]);
    let clock = Pauses::default();
    for label in ["", "-x", "a b", "a/b", "x\n"] {
        let text = launchd_message(jobs(&runner, &clock).loaded(label).unwrap_err());
        assert!(text.ends_with("is not a launchd label"), "{text}");
        assert!(jobs(&runner, &clock).bootout(label).is_err());
    }
    assert!(runner.calls().is_empty());
}

/// Asserts that `with_program` makes every call to the program it names.
#[test]
fn the_program_can_be_named() {
    let runner = FakeRunner::replying(vec![absent(), ok()]);
    let clock = Pauses::default();
    let jobs = jobs(&runner, &clock).with_program("/opt/fake/launchctl");
    jobs.loaded(LABEL).unwrap();
    jobs.bootstrap(Path::new(PLIST)).unwrap();
    for made in runner.calls() {
        assert_eq!(made.program, "/opt/fake/launchctl");
    }
}
