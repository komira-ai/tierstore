//! The launchd service over a recording runner: the exact `launchctl` calls of a start and a
//! stop, the wait for the domain to let the service go, and every way a call can fail.

use std::cell::Cell;
use std::path::Path;
use std::time::Duration;

use super::super::ports::{Clock, PortError, PortKind, Service};
use super::testing::{FakeRunner, exit, signalled};
use super::{
    Call, LAUNCHCTL_NOT_FOUND, LAUNCHCTL_PROGRAM, LAUNCHD_LABEL, LAUNCHD_LINKS_DIR, LAUNCHD_PLIST,
    LAUNCHD_PROGRAM, LaunchdService, LaunchdTimes, RELEASE_BINARY, RunError, bootout_args,
    bootstrap_args, kickstart_args, print_args,
};

const CALL: Duration = Duration::from_secs(11);
const BOOTOUT: Duration = Duration::from_secs(13);
const POLL: Duration = Duration::from_millis(17);
const GONE: u32 = 3;
const TARGET: &str = "system/com.komira.tierstore";

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

fn service<'a>(runner: &'a FakeRunner, clock: &'a Pauses) -> LaunchdService<'a> {
    LaunchdService::new(runner, clock, times())
}

fn loaded() -> Result<super::Output, RunError> {
    exit(
        0,
        "system/com.komira.tierstore = {\n\tstate = running\n}\n",
        "",
    )
}

fn absent() -> Result<super::Output, RunError> {
    exit(
        LAUNCHCTL_NOT_FOUND,
        "",
        "Could not find service \"com.komira.tierstore\" in domain for system",
    )
}

fn ok() -> Result<super::Output, RunError> {
    exit(0, "", "")
}

fn call(args: &[&str], timeout: Duration) -> Call {
    Call::new(LAUNCHCTL_PROGRAM, args, timeout)
}

fn units_message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Units,
            message,
        } => message,
        other => panic!("not a units failure: {other:?}"),
    }
}

/// Asserts the names the service and its packaging share: the label, the plist under
/// `/Library/LaunchDaemons`, `/bin/launchctl`, and the program as the binary of the release
/// `current` names in the links directory; and the argument vectors of the four calls. It would
/// catch a label, a path or a verb that the plist or launchd would not recognise.
#[test]
fn the_names_and_the_argument_vectors_are_launchds() {
    assert_eq!(LAUNCHD_LABEL, "com.komira.tierstore");
    assert_eq!(
        LAUNCHD_PLIST,
        "/Library/LaunchDaemons/com.komira.tierstore.plist"
    );
    assert_eq!(LAUNCHCTL_PROGRAM, "/bin/launchctl");
    assert_eq!(
        Path::new(LAUNCHD_PROGRAM),
        Path::new(LAUNCHD_LINKS_DIR)
            .join("current")
            .join(RELEASE_BINARY)
    );
    assert_eq!(print_args(LAUNCHD_LABEL), ["print", TARGET]);
    assert_eq!(bootout_args(LAUNCHD_LABEL), ["bootout", TARGET]);
    assert_eq!(
        bootstrap_args(LAUNCHD_PLIST),
        ["bootstrap", "system", LAUNCHD_PLIST]
    );
    assert_eq!(kickstart_args(LAUNCHD_LABEL), ["kickstart", TARGET]);
}

/// Asserts that `start` of a service the system domain does not have is `print` (113) and then
/// `bootstrap system <plist>`, each with the call's time limit, and nothing else. It would catch
/// a kickstart of a job that is not loaded, which launchd refuses.
#[test]
fn start_bootstraps_a_service_the_domain_does_not_have() {
    let runner = FakeRunner::replying(vec![absent(), ok()]);
    let clock = Pauses::default();
    service(&runner, &clock).start().unwrap();
    assert_eq!(
        runner.calls(),
        [
            call(&["print", TARGET], CALL),
            call(&["bootstrap", "system", LAUNCHD_PLIST], CALL),
        ]
    );
}

/// Asserts that `start` of a loaded service is `print` (0) and then `kickstart system/<label>`,
/// which starts it if it is not running and does nothing if it runs. It would catch a second
/// bootstrap of a loaded job, which launchd refuses.
#[test]
fn start_kickstarts_a_loaded_service() {
    let runner = FakeRunner::replying(vec![loaded(), ok()]);
    let clock = Pauses::default();
    service(&runner, &clock).start().unwrap();
    assert_eq!(
        runner.calls(),
        [
            call(&["print", TARGET], CALL),
            call(&["kickstart", TARGET], CALL),
        ]
    );
}

/// Asserts that `stop` of a loaded service is `print`, `bootout system/<label>` with the longer
/// time limit of a drain, and then `print` until the domain no longer has the service, one pause
/// of the poll between two looks. It would catch a stop that returns while launchd still has the
/// job, after which a `bootstrap` fails.
#[test]
fn stop_boots_out_and_waits_until_the_domain_lets_go() {
    let runner = FakeRunner::replying(vec![loaded(), ok(), loaded(), absent()]);
    let clock = Pauses::default();
    service(&runner, &clock).stop().unwrap();
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

/// Asserts that `stop` of a service the domain does not have is one `print` and nothing else.
/// It would catch a bootout of an absent job, which launchd answers with an error.
#[test]
fn stop_of_a_service_the_domain_does_not_have_does_nothing() {
    let runner = FakeRunner::replying(vec![absent()]);
    let clock = Pauses::default();
    service(&runner, &clock).stop().unwrap();
    assert_eq!(runner.calls(), [call(&["print", TARGET], CALL)]);
}

/// Asserts that a `bootout` that fails while the service goes anyway (it exited on its own in
/// between) is no error, and that one that fails while the service stays is the error, with the
/// program, the status and its standard error in the message. It would catch a stop that
/// reports a failure for a service that is gone, and one that hides why it is not.
#[test]
fn a_failed_bootout_is_an_error_only_while_the_service_stays() {
    let failed = || exit(5, "", "Boot-out failed: 5: Input/output error");
    let runner = FakeRunner::replying(vec![loaded(), failed(), absent()]);
    let clock = Pauses::default();
    service(&runner, &clock).stop().unwrap();
    let mut replies = vec![loaded(), failed()];
    replies.extend((0..GONE).map(|_| loaded()));
    let runner = FakeRunner::replying(replies);
    let error = service(&runner, &clock).stop().unwrap_err();
    assert_eq!(
        units_message(error),
        "/bin/launchctl bootout system/com.komira.tierstore: exited 5: Boot-out failed: 5: Input/output error"
    );
}

/// Asserts that a service that is still loaded after a `bootout` that succeeded, through every
/// look, fails the stop with a message that says so, after exactly the configured number of
/// looks. It would catch a stop that waits for ever, and one that gives up silently.
#[test]
fn a_service_that_stays_loaded_fails_the_stop() {
    let mut replies = vec![loaded(), ok()];
    replies.extend((0..GONE).map(|_| loaded()));
    let runner = FakeRunner::replying(replies);
    let clock = Pauses::default();
    let error = service(&runner, &clock).stop().unwrap_err();
    assert_eq!(
        units_message(error),
        "system/com.komira.tierstore is still loaded after bootout and 3 looks"
    );
    assert_eq!(runner.calls().len(), 2 + GONE as usize);
    assert_eq!(clock.count.get(), GONE);
}

/// Asserts that a `print` that neither finds nor misses the service (any status but 0 and
/// 113, a signal, a program that cannot run or that runs past its time) fails the start and the
/// stop before anything else is called, and that a `bootstrap` or `kickstart` that fails fails
/// the start. It would catch a status read as "absent" that is something else.
#[test]
fn every_other_answer_of_launchctl_is_a_failure() {
    let clock = Pauses::default();
    for reply in [
        exit(1, "", "Unrecognized target specifier."),
        signalled(),
        Err(RunError::Spawn("No such file or directory".to_owned())),
        Err(RunError::Timeout(CALL)),
    ] {
        for stop in [false, true] {
            let runner = FakeRunner::replying(vec![reply.clone()]);
            let service = service(&runner, &clock);
            let result = if stop {
                service.stop()
            } else {
                service.start()
            };
            let message = units_message(result.unwrap_err());
            assert!(
                message.starts_with("/bin/launchctl print system/com.komira.tierstore: "),
                "{message}"
            );
            assert_eq!(runner.calls().len(), 1);
        }
    }
    for (first, verb) in [(absent(), "bootstrap"), (loaded(), "kickstart")] {
        let runner =
            FakeRunner::replying(vec![first, exit(37, "", "Operation already in progress")]);
        let error = service(&runner, &clock).start().unwrap_err();
        let message = units_message(error);
        assert!(message.contains(&format!("{verb} ")), "{message}");
        assert!(
            message.ends_with("exited 37: Operation already in progress"),
            "{message}"
        );
    }
}

/// Asserts that `with_program` makes every call to the program it names. It would catch a
/// program that is ignored, which the conformance tests rely on.
#[test]
fn the_program_can_be_named() {
    let runner = FakeRunner::replying(vec![absent(), ok()]);
    let clock = Pauses::default();
    service(&runner, &clock)
        .with_program("/opt/fake/launchctl")
        .start()
        .unwrap();
    for made in runner.calls() {
        assert_eq!(made.program, "/opt/fake/launchctl");
    }
}
