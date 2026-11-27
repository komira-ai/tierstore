//! The vendored protocol of a mount provider: the names the crate exports are the proto's, and a
//! request's bytes on the wire are the ones jobscheduler's agent sends for the same fields.

use prost::Message as _;
use tierstore_trees_api::provider::v1::{
    KeptAttempt, MountMode, PrepareRequest, ReleaseRequest, Source, SweepRequest,
};
use tierstore_trees_api::provider::{METHODS, PACKAGE, SERVICE, path};

/// Asserts the consts are what `provider.proto` declares, so that a path built from them is the
/// path the generated server serves.
#[test]
fn names_are_the_ones_of_the_provider_proto() {
    let set = protox::compile(
        ["jobscheduler/provider/v1/provider.proto"],
        [concat!(env!("CARGO_MANIFEST_DIR"), "/proto")],
    )
    .expect("the proto compiles");
    let file = &set.file[0];
    assert_eq!(file.package(), PACKAGE);
    assert_eq!(file.service.len(), 1);
    let service = &file.service[0];
    assert_eq!(service.name(), SERVICE);
    let methods: Vec<&str> = service.method.iter().map(|m| m.name()).collect();
    assert_eq!(methods, METHODS);
    assert_eq!(
        path("Prepare"),
        "/jobscheduler.provider.v1.MountProvider/Prepare"
    );
}

/// A `Prepare` with every field set, as protobuf writes it: the source (agent, run, index), the
/// ref, the mode and the host, each by its number.
const PREPARE: &[u8] = &[
    0x0a, 0x0c, // source, 12 bytes
    0x0a, 0x02, b'a', b'1', // agent
    0x12, 0x04, b'j', b's', b'.', b'7', // run
    0x18, 0x02, // index
    0x12, 0x08, b'c', b'a', b's', b'/', b'l', b'/', b'a', b'b', // ref
    0x18, 0x01, // mode READ_ONLY
    0x22, 0x01, b'h', // host
];

fn prepare() -> PrepareRequest {
    PrepareRequest {
        source: Some(Source {
            agent: "a1".into(),
            run: "js.7".into(),
            index: 2,
            attempt: 0,
        }),
        r#ref: "cas/l/ab".into(),
        mode: MountMode::ReadOnly.into(),
        host: "h".into(),
    }
}

/// Asserts the request encodes to exactly the golden bytes and decodes from them, and that a field
/// a later agent adds is skipped.
#[test]
fn a_prepare_is_the_golden_bytes_both_ways() {
    assert_eq!(prepare().encode_to_vec(), PREPARE);
    assert_eq!(PrepareRequest::decode(PREPARE).expect("decodes"), prepare());
    let mut later = PREPARE.to_vec();
    later.extend_from_slice(&[0x48, 0x05]);
    assert_eq!(
        PrepareRequest::decode(&later[..]).expect("decodes"),
        prepare()
    );
    let modes = [
        MountMode::Unspecified,
        MountMode::ReadOnly,
        MountMode::ReadWrite,
        MountMode::CopyOnWrite,
    ];
    let numbers: Vec<i32> = modes.iter().map(|mode| *mode as i32).collect();
    assert_eq!(numbers, [0, 1, 2, 3]);
}

/// A `Sweep` names its agent and the runs it keeps, each a string of field 2.
#[test]
fn a_sweep_is_the_golden_bytes() {
    let sweep = SweepRequest {
        agent: "a".into(),
        keep: vec!["js.1".into(), "js.2".into()],
        keep_attempts: Vec::new(),
    };
    let bytes = [
        0x0a, 0x01, b'a', 0x12, 0x04, b'j', b's', b'.', b'1', 0x12, 0x04, b'j', b's', b'.', b'2',
    ];
    assert_eq!(sweep.encode_to_vec(), bytes);
}

/// A `Release` of attempt 3 of a source, as protobuf writes it: the source with the attempt as a
/// varint of field 4 after the index.
const RELEASE_ATTEMPT: &[u8] = &[
    0x0a, 0x0e, // source, 14 bytes
    0x0a, 0x02, b'a', b'1', // agent
    0x12, 0x04, b'j', b's', b'.', b'7', // run
    0x18, 0x02, // index
    0x20, 0x03, // attempt
];

/// Asserts the attempt on the wire both ways: a source that names one is the golden bytes, the
/// source's bytes before the field followed by field 4 as a varint (what a provider that does not
/// know the field skips, as `a_prepare_is_the_golden_bytes_both_ways` shows of an unknown field); a
/// source that names none (attempt 0) writes no byte for it, so an agent that counts no attempts
/// sends exactly what it sent before the field; and the bytes of such an agent decode as attempt 0.
#[test]
fn a_sources_attempt_is_field_4_and_absent_when_0() {
    let source = |attempt| Source {
        agent: "a1".into(),
        run: "js.7".into(),
        index: 2,
        attempt,
    };
    let release = ReleaseRequest {
        source: Some(source(3)),
    };
    assert_eq!(release.encode_to_vec(), RELEASE_ATTEMPT);
    assert_eq!(
        ReleaseRequest::decode(RELEASE_ATTEMPT).expect("decodes"),
        release
    );
    let before = &RELEASE_ATTEMPT[2..14];
    let mut unnumbered = vec![0x0a, 0x0c];
    unnumbered.extend_from_slice(before);
    let none = ReleaseRequest {
        source: Some(source(0)),
    };
    assert_eq!(none.encode_to_vec(), unnumbered);
    assert_eq!(
        ReleaseRequest::decode(&unnumbered[..]).expect("decodes"),
        none
    );
    assert_eq!(
        Source::decode(&RELEASE_ATTEMPT[2..])
            .expect("decodes")
            .attempt,
        3
    );
    assert_eq!(Source::decode(before).expect("decodes"), source(0));
}

/// A `Sweep` names the runs it keeps from an attempt on as messages of field 3, each its run (field
/// 1) and its attempt (field 2); without any, a sweep is the bytes it was before the field.
#[test]
fn a_sweep_with_kept_attempts_is_the_golden_bytes() {
    let sweep = SweepRequest {
        agent: "a".into(),
        keep: vec!["js.1".into()],
        keep_attempts: vec![KeptAttempt {
            run: "js.1".into(),
            attempt: 2,
        }],
    };
    let bytes = [
        0x0a, 0x01, b'a', // agent
        0x12, 0x04, b'j', b's', b'.', b'1', // keep
        0x1a, 0x08, // keep_attempts, 8 bytes
        0x0a, 0x04, b'j', b's', b'.', b'1', // run
        0x10, 0x02, // attempt
    ];
    assert_eq!(sweep.encode_to_vec(), bytes);
    assert_eq!(SweepRequest::decode(&bytes[..]).expect("decodes"), sweep);
    let old = SweepRequest {
        keep_attempts: Vec::new(),
        ..sweep
    };
    assert_eq!(old.encode_to_vec(), &bytes[..9]);
    assert_eq!(SweepRequest::decode(&bytes[..9]).expect("decodes"), old);
}
