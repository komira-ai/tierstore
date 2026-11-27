//! The name of a source and its path under the mount: what a request may name, the path it is
//! placed at, and the reading of a path back to its source.

use tierstore_trees_api::provider::v1::Source;

use super::ProviderError;
use super::names::{MAX_NAME_BYTES, ROOT, SourceId, agent_dir, source_of};
use crate::server::local_cache::trees::Location;

fn id(agent: &str, run: &str, index: u32) -> SourceId {
    SourceId {
        agent: agent.to_owned(),
        run: run.to_owned(),
        index,
        attempt: 0,
    }
}

/// Asserts the path of a source: the root, the hex of the agent and of the run, and the index, and
/// that it is a location the tree store takes as it is, the longest names included.
#[test]
fn a_source_is_placed_at_the_hex_of_its_names() {
    assert_eq!(ROOT, "mount-provider");
    assert_eq!(MAX_NAME_BYTES, 120);
    let source = id("agent-1", "js.7", 3);
    assert_eq!(source.path(), "mount-provider/6167656e742d31/6a732e37/3");
    assert_eq!(
        source.location(),
        Location::Fs {
            path: source.path()
        }
    );
    assert_eq!(agent_dir("agent-1"), "mount-provider/6167656e742d31/");
    let longest = id(&"a".repeat(120), &"/.\0".repeat(40), u32::MAX);
    assert_eq!(Location::fs(&longest.path()), Ok(longest.location()));
}

/// Asserts that a request's source is taken with its names as they are, and refused by name when
/// it is absent or a name is empty or over the bound.
#[test]
fn a_source_is_refused_without_names_or_with_long_ones() {
    let source = |agent: &str, run: &str| {
        Some(Source {
            agent: agent.into(),
            run: run.into(),
            index: 9,
            attempt: 0,
        })
    };
    assert_eq!(SourceId::of(source("a", "js.1")), Ok(id("a", "js.1", 9)));
    let longest = "x".repeat(MAX_NAME_BYTES);
    assert!(SourceId::of(source(&longest, &longest)).is_ok());
    let too_long = "x".repeat(MAX_NAME_BYTES + 1);
    let refused = |why: &str| Err(ProviderError::Invalid(why.into()));
    assert_eq!(SourceId::of(None), refused("no source"));
    assert_eq!(
        SourceId::of(source("", "js.1")),
        refused("the agent is 1 to 120 bytes")
    );
    assert_eq!(
        SourceId::of(source(&too_long, "js.1")),
        refused("the agent is 1 to 120 bytes")
    );
    assert_eq!(
        SourceId::of(source("a", "")),
        refused("the run is 1 to 120 bytes")
    );
    assert_eq!(
        SourceId::of(source("a", &too_long)),
        refused("the run is 1 to 120 bytes")
    );
}

/// Asserts that a path is read back to exactly the source it is the path of, and that every other
/// path is not a source of the agent: another agent's, one above a source, a run that is not hex,
/// not lowercase or not UTF-8, and an index that is not a number or not written as one.
#[test]
fn only_a_sources_own_path_is_read_back_to_it() {
    let source = id("agent-1", "js.7", 3);
    assert_eq!(source_of(&source.path(), "agent-1"), Some(source.clone()));
    let dir = agent_dir("agent-1");
    for path in [
        source.path().replace("6167656e742d31", "6167656e742d32"),
        format!("{dir}6a732e37"),
        format!("{dir}zz/3"),
        format!("{dir}6a732/3"),
        format!("{dir}6A732E37/3"),
        format!("{dir}ff/3"),
        format!("{dir}6a732e37/x"),
        format!("{dir}6a732e37/03"),
        format!("{dir}6a732e37/3/4"),
        "trees/abc".to_owned(),
    ] {
        assert_eq!(source_of(&path, "agent-1"), None, "{path}");
    }
}

/// The source `index` of the attempt `attempt` of `run` of `agent`.
fn attempt(agent: &str, run: &str, index: u32, attempt: u64) -> SourceId {
    SourceId {
        attempt,
        ..id(agent, run, index)
    }
}

/// Asserts that a source that names its attempt is placed in a directory of that attempt under its
/// run's (`a` and the attempt in decimal), the cache's run being that directory, that the same
/// source without an attempt is at the path of before, that the largest attempt with the longest
/// names is still a location the tree store takes, and that a request's attempt is taken as it is.
#[test]
fn an_attempt_has_a_directory_of_its_own_under_the_run() {
    let source = attempt("agent-1", "js.7", 3, 12);
    assert_eq!(
        source.path(),
        "mount-provider/6167656e742d31/6a732e37/a12/3"
    );
    assert_eq!(
        source.run_dir(),
        "mount-provider/6167656e742d31/6a732e37/a12"
    );
    assert_eq!(source.unnumbered(), id("agent-1", "js.7", 3));
    assert_eq!(
        source.unnumbered().path(),
        "mount-provider/6167656e742d31/6a732e37/3"
    );
    let longest = attempt(&"a".repeat(120), &"/.\0".repeat(40), u32::MAX, u64::MAX);
    assert_eq!(Location::fs(&longest.path()), Ok(longest.location()));
    let request = Some(Source {
        agent: "agent-1".into(),
        run: "js.7".into(),
        index: 3,
        attempt: 12,
    });
    assert_eq!(SourceId::of(request), Ok(source));
}

/// Asserts that the path of a source that names its attempt is read back to exactly that source,
/// and that a path whose attempt is not `a` and a decimal number of 1 or more written as one, or
/// that has no index under the attempt or more below it, is not a source of the agent.
#[test]
fn only_an_attempts_own_path_is_read_back_to_it() {
    let source = attempt("agent-1", "js.7", 3, 12);
    assert_eq!(source_of(&source.path(), "agent-1"), Some(source));
    let max = attempt("agent-1", "js.7", 0, u64::MAX);
    assert_eq!(source_of(&max.path(), "agent-1"), Some(max));
    let run = format!("{}6a732e37", agent_dir("agent-1"));
    for path in [
        format!("{run}/a0/3"),
        format!("{run}/a012/3"),
        format!("{run}/a/3"),
        format!("{run}/ax/3"),
        format!("{run}/b12/3"),
        format!("{run}/12/3"),
        format!("{run}/a+12/3"),
        format!("{run}/a18446744073709551616/3"),
        format!("{run}/a12"),
        format!("{run}/a12/"),
        format!("{run}/a12/3/4"),
    ] {
        assert_eq!(source_of(&path, "agent-1"), None, "{path}");
    }
}
