//! The update hook's record across the release before this one and this branch: the previous
//! release's hook (`tierstore_prev`, the real library of its commit) reads, and checks, the
//! record this branch's hook writes for an update in flight, on systemd and on launchd, and this
//! branch's hook reads what the previous release's hook wrote.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::PathBuf;

use tierstore::server::update_hook::{Instance, Marker, Mode, Record};
use tierstore_prev::server::update_hook::{Marker as PrevMarker, SingleRecord};

fn release(name: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/hostkeeper/packages/tierstore/{name}"))
}

/// An update in flight of this branch's hook, with `markers` done.
fn updating(markers: &[Marker]) -> Record {
    Record {
        updating: true,
        markers: markers.iter().copied().collect(),
        from: Some(release("0.4.0")),
        to: Some(release("0.5.0")),
        version: Some("0.5.0+0123456789ab".to_owned()),
        ..Record::idle(None)
    }
}

/// The words of the markers of the record this branch wrote, to compare with the previous
/// release's.
fn words(record: &Record) -> Vec<&'static str> {
    record
        .markers
        .iter()
        .map(|marker| marker.as_str())
        .collect()
}

fn prev_words(record: &SingleRecord) -> Vec<&'static str> {
    record
        .markers
        .iter()
        .map(|marker| marker.as_str())
        .collect()
}

/// Asserts that the previous release's hook reads the record of an update in flight on systemd,
/// with `live`, `next` and `mode` set, at each stage of the update: it decodes, passes its own
/// `check`, and sees the same update (in flight, the same releases, version and markers), the
/// three new keys ignored. Catches a key of the new record that the previous hook reads wrongly
/// or refuses, which would break a rollback of the package in the middle of an update.
#[test]
fn the_previous_hook_reads_a_systemd_update_in_flight() {
    let stages: [&[Marker]; 4] = [
        &[],
        &[Marker::Swapped],
        &[Marker::Swapped, Marker::Verified],
        &[Marker::Swapped, Marker::Verified, Marker::Stopping],
    ];
    for markers in stages {
        for mode in [Mode::Handover, Mode::Restart] {
            let record = Record {
                live: Some(Instance::A),
                next: Some(Instance::B),
                mode: Some(mode),
                ..updating(markers)
            };
            assert_eq!(record.check(), Ok(()));
            let read = SingleRecord::decode(&record.encode()).unwrap();
            assert_eq!(read.check(), Ok(()), "{markers:?}");
            assert!(read.updating);
            assert_eq!(read.from, record.from);
            assert_eq!(read.to, record.to);
            assert_eq!(read.version, record.version);
            assert_eq!(prev_words(&read), words(&record));
            assert_eq!(read.committed(), record.committed());
        }
    }
}

/// Asserts that the previous release's hook reads the record of the launchd arm (no `live`,
/// `next` or `mode`) and the record of a systemd host at rest and after a commit, which it checks
/// as it checks its own. Catches a record at rest or after `cleanup` that the previous hook
/// takes for corrupt.
#[test]
fn the_previous_hook_reads_a_launchd_record_and_a_record_at_rest() {
    let launchd = updating(&[Marker::Swapped, Marker::Verified]);
    let read = SingleRecord::decode(&launchd.encode()).unwrap();
    assert_eq!(read.check(), Ok(()));
    assert_eq!(prev_words(&read), ["swapped", "verified"]);

    let rest = Record::idle(Some(Instance::B));
    let read = SingleRecord::decode(&rest.encode()).unwrap();
    assert_eq!(read.check(), Ok(()));
    assert!(!read.updating && read.markers.is_empty());

    let committed = Record {
        updating: false,
        from: None,
        live: Some(Instance::B),
        ..updating(&[Marker::Swapped, Marker::Verified, Marker::Stopping])
    };
    assert_eq!(committed.check(), Ok(()));
    let read = SingleRecord::decode(&committed.encode()).unwrap();
    assert_eq!(read.check(), Ok(()));
    assert!(read.committed() && !read.updating);
    assert_eq!(read.to, committed.to);
}

/// Asserts that this branch's hook reads what the previous release's hook writes: an update in
/// flight at each stage, and a host at rest. They decode, pass this hook's `check`, and name the
/// same update, with `live`, `next` and `mode` absent. Catches a record of the previous release
/// that this hook refuses after an upgrade in the middle of an update.
#[test]
fn this_hook_reads_what_the_previous_hook_wrote() {
    let started = SingleRecord::updating(
        release("0.4.0"),
        release("0.5.0"),
        "0.5.0+0123456789ab".to_owned(),
    );
    for record in [
        started.clone(),
        started.with(PrevMarker::Swapped),
        started.with(PrevMarker::Swapped).with(PrevMarker::Verified),
        started
            .with(PrevMarker::Swapped)
            .with(PrevMarker::Verified)
            .with(PrevMarker::Stopping),
    ] {
        let read = Record::decode(&record.encode()).unwrap();
        assert_eq!(read.check(), Ok(()));
        assert!(read.updating);
        assert_eq!(read.from, record.from);
        assert_eq!(read.to, record.to);
        assert_eq!(read.version, record.version);
        assert_eq!(words(&read), prev_words(&record));
        assert_eq!((read.live, read.next, read.mode), (None, None, None));
    }
    let read = Record::decode(&SingleRecord::idle().encode()).unwrap();
    assert_eq!(read, Record::idle(None));
    assert_eq!(read.check(), Ok(()));
}
