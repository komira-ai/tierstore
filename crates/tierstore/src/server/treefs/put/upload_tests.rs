//! The upload of a commit: what each answer of the socket comes to, and the pauses of the
//! back-off. The tries over a socket, and the deadline, are asserted through the namespace in
//! `space_commit_tests.rs`.

use std::time::Duration;

use super::super::commit::PutOutcome;
use super::super::names::Sha256Digest;
use super::{Tried, pause_before, read_answer};

/// Asserts what each line the socket may answer a `put` with comes to: the two successes, the
/// three final refusals, `upstream` asked again, and anything else (a word this release does not
/// know, `stage`, JSON without the word, the socket's own refusal, a line that is not JSON) a
/// failure not asked again.
#[test]
fn each_answer_of_the_socket_comes_to_its_outcome() {
    for (line, tried) in [
        (
            r#"{"ok":true,"created":true}"#,
            Tried::Done(PutOutcome::Created),
        ),
        (
            r#"{"ok":true,"created":false}"#,
            Tried::Done(PutOutcome::AlreadyStored),
        ),
        (r#"{"ok":true}"#, Tried::Done(PutOutcome::AlreadyStored)),
        (
            r#"{"ok":false,"error":"exists_different"}"#,
            Tried::Done(PutOutcome::ExistsDifferent),
        ),
        (
            r#"{"ok":false,"error":"checksum"}"#,
            Tried::Done(PutOutcome::Checksum),
        ),
        (
            r#"{"ok":false,"error":"toolarge"}"#,
            Tried::Done(PutOutcome::TooLarge),
        ),
        (r#"{"ok":false,"error":"upstream"}"#, Tried::Again),
        (
            r#"{"ok":false,"error":"stage"}"#,
            Tried::Done(PutOutcome::Upstream),
        ),
        (
            r#"{"ok":false,"error":"later"}"#,
            Tried::Done(PutOutcome::Upstream),
        ),
        (r#"{"ok":false}"#, Tried::Done(PutOutcome::Upstream)),
        ("refused malformed", Tried::Done(PutOutcome::Upstream)),
        ("", Tried::Done(PutOutcome::Upstream)),
    ] {
        assert_eq!(read_answer(line), tried, "{line}");
    }
}

/// Asserts the pauses of the back-off: from 100 ms, doubled at each try up to 5 s, of which the
/// second half is spread by the digest (none of it for a digest of zeros, all of it for one of
/// 0xff bytes), the same for the same digest and try, and whole milliseconds at any count of
/// tries.
#[test]
fn the_pause_doubles_to_a_ceiling_and_is_spread_by_the_digest() {
    let zeros = Sha256Digest([0; 32]);
    let ones = Sha256Digest([0xff; 32]);
    let low: Vec<u64> = (0..8)
        .map(|tries| millis(pause_before(tries, &zeros)))
        .collect();
    let high: Vec<u64> = (0..8)
        .map(|tries| millis(pause_before(tries, &ones)))
        .collect();
    assert_eq!(low, [50, 100, 200, 400, 800, 1600, 2500, 2500]);
    assert_eq!(high, [100, 200, 400, 800, 1600, 3200, 5000, 5000]);
    let mut mixed = [0_u8; 32];
    mixed[0] = 128;
    mixed[31] = 255;
    let mixed = Sha256Digest(mixed);
    let pause = pause_before(0, &mixed);
    assert!(pause > Duration::from_millis(50) && pause < Duration::from_millis(100));
    assert_eq!(pause, pause_before(0, &mixed));
    assert_eq!(pause_before(u32::MAX, &ones), Duration::from_secs(5));
    assert_eq!(pause_before(40, &zeros).subsec_nanos() % 1_000_000, 0);
}

/// The whole milliseconds of `duration`.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap()
}

/// Asserts that a try whose answer does not come waits no longer than its commit's deadline
/// leaves (here 5 ms of a 30 s answer timeout), and that the upload then ends `Upstream` with no
/// second try, since no pause fits before the deadline.
#[test]
fn a_try_waits_for_its_answer_no_longer_than_the_deadline_leaves() {
    let rig = super::super::testing::SpaceRig::new("put-upload-left");
    let socket = rig.rig.root.path().join("fill.sock");
    let clock = rig.rig.clock.clone();
    let uploader = super::Uploader::new(
        &socket,
        Duration::from_secs(30),
        clock.clone(),
        std::sync::Arc::new(|_| {}),
    );
    rig.puts.hold();
    let staged = rig.rig.root.path().join("staged");
    std::fs::write(&staged, b"x").unwrap();
    let outcome = uploader.put(
        ("cas/b/k", super::super::testing::sha(b"x"), 1),
        &staged,
        clock.now() + 5,
    );
    assert_eq!(outcome, PutOutcome::Upstream);
    assert_eq!(rig.puts.count(), 1);
    rig.puts.let_go();
}
