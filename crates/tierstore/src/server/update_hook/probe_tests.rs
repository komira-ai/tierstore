//! What is read off an instance's answers.

use super::{Build, CacheMode, PortError, PortKind, Reply, build_of, readiness};

fn reply(status: u16, body: &str) -> Result<Reply, PortError> {
    Ok(Reply {
        status,
        body: body.to_owned(),
    })
}

/// Asserts the reading of `/readyz`: the three bodies of a 200, and no mode for anything else.
/// It would catch a draining instance (503) or an answer that is not tierstore's taken for ready.
#[test]
fn readiness_is_read_from_the_status_and_the_body() {
    assert_eq!(
        readiness(&reply(200, "ready read-only")),
        Some(CacheMode::ReadOnly)
    );
    assert_eq!(
        readiness(&reply(200, "ready read-write\n")),
        Some(CacheMode::ReadWrite)
    );
    assert_eq!(readiness(&reply(200, "ready")), Some(CacheMode::Plain));
    assert_eq!(readiness(&reply(200, "ready read-sideways")), None);
    assert_eq!(readiness(&reply(503, "draining")), None);
    assert_eq!(readiness(&reply(200, "")), None);
    let refused = Err(PortError::failed(PortKind::Instances, "refused"));
    assert_eq!(readiness(&refused), None);
}

const GAUGE: &str = "# TYPE tierstore_build_info gauge\n\
tierstore_build_info{version=\"0.4.0\",commit=\"abc123\",slot=\"9302\"} 1\n";

/// Asserts that the gauge is found among other lines and its labels are read, and that a gauge
/// without a commit or a slot, or with a slot that is no number, still reads. It would catch a
/// reader that needs the whole gauge or takes a slot from the wrong label.
#[test]
fn the_build_gauge_is_read_from_the_metrics() {
    let text = format!("tierstore_hits_total 3\n{GAUGE}tierstore_other 1\n");
    let build = build_of(&text).unwrap();
    assert_eq!(
        build,
        Build {
            version: "0.4.0".into(),
            commit: Some("abc123".into()),
            slot: Some(9302)
        }
    );
    let bare = build_of("tierstore_build_info{version=\"1\"} 1\n").unwrap();
    assert_eq!((bare.commit, bare.slot), (None, None));
    let odd = build_of("tierstore_build_info{version=\"1\",slot=\"x\"} 1\n").unwrap();
    assert_eq!(odd.slot, None);
}

/// Asserts what the reader refuses: no gauge, no version label, an unclosed label or brace.
#[test]
fn a_metrics_text_without_a_usable_gauge_gives_none() {
    assert_eq!(build_of("tierstore_hits_total 3\n"), None);
    assert_eq!(build_of(""), None);
    assert_eq!(build_of("tierstore_build_info{commit=\"a\"} 1\n"), None);
    assert_eq!(build_of("tierstore_build_info{version=\"1} 1\n"), None);
    assert_eq!(build_of("tierstore_build_info{version=\"1\" 1\n"), None);
}

/// Asserts when a build is the instance on a slot with a version: the slot must be named and
/// equal, and the version is the crate version or that, `+` and the commit. It would catch a
/// probe through the public port that takes the old instance's answer for the new one's, which
/// would let a cutover pass with traffic still on the old release.
#[test]
fn a_build_is_an_instance_only_if_slot_and_version_agree() {
    let build = build_of(GAUGE).unwrap();
    assert!(build.is(9302, "0.4.0"));
    assert!(build.is(9302, "0.4.0+abc123"));
    assert!(!build.is(9301, "0.4.0"));
    assert!(!build.is(9302, "0.3.0"));
    assert!(!build.is(9302, "0.4.0+other"));
    let no_slot = build_of("tierstore_build_info{version=\"0.4.0\"} 1\n").unwrap();
    assert!(!no_slot.is(9302, "0.4.0"));
    assert!(!no_slot.is(9302, "0.4.0+abc123"));
}
