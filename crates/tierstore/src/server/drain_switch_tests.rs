//! The note a node leaves of its late requests as it exits, and the test-only switch that takes
//! the drain's two properties away: what turns it on, and that nothing a release is built from
//! does.

use super::drain::late_note;

/// The crate's manifest.
const MANIFEST: &str = include_str!("../../Cargo.toml");
/// The workflow that builds the release binaries.
const ARTIFACTS: &str = include_str!("../../../../.github/workflows/artifacts.yml");
/// The workflow of the pull-request checks, whose `handover-t1` job builds the switch.
const CI: &str = include_str!("../../../../.github/workflows/ci.yml");

/// The switch.
const SWITCH: &str = "test-drain-off";

/// Asserts that the note says nothing when no request came late, and else one line for each front
/// that counted one, with the count and the front's name. It would catch a note that hides a late
/// request of the OCI front, or one that speaks when there is nothing to say (a drain that ended
/// clean leaves stderr as it was).
#[test]
fn the_note_names_each_front_that_counted_a_late_request() {
    assert_eq!(late_note(0, 0), "");
    assert_eq!(
        late_note(2, 0),
        "tierstore: 2 requests came too late in the drain of the S3 front\n"
    );
    assert_eq!(
        late_note(0, 1),
        "tierstore: 1 requests came too late in the drain of the OCI front\n"
    );
    assert_eq!(
        late_note(3, 4),
        "tierstore: 3 requests came too late in the drain of the S3 front\n\
         tierstore: 4 requests came too late in the drain of the OCI front\n"
    );
}

/// The features `name` turns on, as the manifest lists them.
fn feature(manifest: &toml::Table, name: &str) -> Vec<String> {
    manifest["features"][name]
        .as_array()
        .unwrap_or_else(|| panic!("no feature {name}"))
        .iter()
        .map(|feature| feature.as_str().expect("a feature name").to_owned())
        .collect()
}

/// Asserts that the build the tests run has the drain, and that nothing a release or an install is
/// built from turns the switch on: it is not a default feature, no other feature turns it on, the
/// crate's dev-dependency on itself (every test and coverage run) leaves it off, and each build of
/// the release workflow names `--features server` alone. The one place that names it is the CI
/// job that builds the binary for the hand-over test. The switch is a feature of the build, not a
/// flag: a binary without it has no way to turn the drain off. It would catch the switch leaking
/// into a release, or into the test run that proves P1 and P2.
#[test]
fn no_release_and_no_test_run_has_the_switch() {
    const { assert!(!cfg!(feature = "test-drain-off")) };
    let manifest: toml::Table = MANIFEST.parse().expect("the manifest");
    assert_eq!(feature(&manifest, SWITCH), ["server"]);
    let features = manifest["features"].as_table().expect("features");
    assert!(!features.contains_key("default"));
    for (name, list) in features {
        let list = list.as_array().expect("a list");
        assert!(
            !list.iter().any(|feature| feature.as_str() == Some(SWITCH)),
            "{name} turns {SWITCH} on"
        );
    }
    let itself = &manifest["dev-dependencies"]["tierstore"]["features"];
    let itself: Vec<&str> = itself
        .as_array()
        .expect("the features of the dev-dependency on itself")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    assert_eq!(itself, ["server", "hook-conformance"]);
    let builds: Vec<&str> = ARTIFACTS
        .lines()
        .filter(|line| line.contains("cargo build"))
        .collect();
    assert_eq!(builds.len(), 2, "{builds:?}");
    for build in builds {
        assert!(
            build.ends_with("-p tierstore --features server --bin tierstore"),
            "{build}"
        );
    }
    assert!(!ARTIFACTS.contains(SWITCH));
    let named: Vec<&str> = CI
        .lines()
        .filter(|line| line.contains(SWITCH) && line.contains("cargo "))
        .collect();
    assert_eq!(named.len(), 1, "the handover-t1 job builds it: {named:?}");
    for line in named {
        assert!(line.contains("--target-dir target/drain-off"), "{line}");
    }
}
