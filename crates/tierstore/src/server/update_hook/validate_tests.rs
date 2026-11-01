//! The rules the record and the params are held to.

use std::collections::BTreeSet;

use super::testing::DECLARATION;
use super::{Declaration, Invalid, Marker, Mode, Params, Record, SCHEMA, check_params, validate};

fn decl() -> Declaration {
    Declaration::parse(DECLARATION).unwrap()
}

fn record(serving: u16, previous: Option<u16>, markers: &[Marker]) -> Record {
    Record {
        schema: SCHEMA,
        serving,
        previous,
        mode: Mode::Handover,
        markers: markers.iter().copied().collect::<BTreeSet<_>>(),
        boot_id: None,
        guard_placed: false,
    }
}

/// Asserts that the records a run of the steps writes pass: nothing in flight, each stage of an
/// update, and the state after the commit. It would catch a rule that refuses what the hook
/// itself writes, which the updater would see as a host nothing can repair.
#[test]
fn the_records_the_steps_write_pass() {
    use Marker::{Stopping, Swapped, Verified};
    let d = decl();
    for r in [
        record(9301, None, &[]),
        record(9302, Some(9301), &[]),
        record(9302, Some(9301), &[Swapped]),
        record(9302, Some(9301), &[Swapped, Verified]),
        Record {
            boot_id: Some("0f3a-91".into()),
            guard_placed: true,
            ..record(9302, Some(9301), &[Swapped, Verified])
        },
        record(9302, Some(9301), &[Swapped, Verified, Stopping]),
        record(9302, None, &[Swapped, Verified, Stopping]),
    ] {
        assert_eq!(validate(&r, &d), Ok(()), "{r:?}");
    }
}

/// Asserts each rule with the record that breaks it. It would catch a port taken from the record
/// without being one of the two declared (the hook would then build a unit name from it), a
/// marker the hook could not have written, and markers left with no update.
#[test]
fn a_record_that_breaks_a_rule_is_refused() {
    use Marker::{Stopping, Swapped, Verified};
    let d = decl();
    let cases = [
        (
            record(9999, None, &[]),
            Invalid::Port {
                field: "serving",
                found: 9999,
            },
        ),
        (
            record(9301, Some(9999), &[]),
            Invalid::Port {
                field: "previous",
                found: 9999,
            },
        ),
        (
            record(9300, None, &[]),
            Invalid::Port {
                field: "serving",
                found: 9300,
            },
        ),
        (record(9301, Some(9301), &[]), Invalid::Same),
        (
            record(9302, Some(9301), &[Verified]),
            Invalid::Marks {
                mark: "verified",
                needs: "swapped",
            },
        ),
        (
            record(9302, Some(9301), &[Swapped, Stopping]),
            Invalid::Marks {
                mark: "stopping",
                needs: "verified",
            },
        ),
        (record(9301, None, &[Swapped]), Invalid::Orphan),
        (
            Record {
                boot_id: Some("boot-1".into()),
                ..record(9302, Some(9301), &[Swapped])
            },
            Invalid::Marks {
                mark: "boot_id",
                needs: "verified",
            },
        ),
        (
            Record {
                boot_id: Some("a b".into()),
                ..record(9302, Some(9301), &[Swapped, Verified])
            },
            Invalid::BootId,
        ),
        (
            Record {
                boot_id: Some(String::new()),
                ..record(9302, Some(9301), &[Swapped, Verified])
            },
            Invalid::BootId,
        ),
        (
            Record {
                guard_placed: true,
                ..record(9301, None, &[])
            },
            Invalid::Marks {
                mark: "guard_placed",
                needs: "previous",
            },
        ),
    ];
    for (r, why) in cases {
        assert_eq!(validate(&r, &d), Err(why), "{r:?}");
    }
}

/// Asserts that the messages name the field or the marker. It would catch an error a person cannot
/// act on.
#[test]
fn the_messages_name_what_is_wrong() {
    assert_eq!(
        Invalid::Port {
            field: "serving",
            found: 7
        }
        .to_string(),
        "serving is 7, not a declared port"
    );
    assert!(Invalid::Same.to_string().contains("same port"));
    assert!(Invalid::Orphan.to_string().contains("no update in flight"));
    assert!(Invalid::Staged.to_string().contains("state directory"));
    assert_eq!(
        Invalid::Marks {
            mark: "a",
            needs: "b"
        }
        .to_string(),
        "a is set but b is not"
    );
}

/// Asserts that a staged release inside the state directory is refused and one beside it is not.
/// It would catch a release that the service-writable area could swap.
#[test]
fn the_staged_release_is_not_in_the_state_directory() {
    let text = |dir: &str| {
        format!(
            "promotion_bound_secs = 30\n\"updater.staged_release\" = \"{dir}\"\n\"updater.target_version\" = \"1\"\n"
        )
    };
    let d = decl();
    let inside = Params::parse(&text("/var/lib/updater/hooks/tierstore/slots/9301")).unwrap();
    assert_eq!(check_params(&inside, &d), Err(Invalid::Staged));
    let beside = Params::parse(&text("/var/lib/updater/hooks/tierstore2")).unwrap();
    assert_eq!(check_params(&beside, &d), Ok(()));
}
