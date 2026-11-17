//! The vendored protocol of a mount provider: the names the crate exports are the proto's, and a
//! request's bytes on the wire are the ones jobscheduler's agent sends for the same fields.

use prost::Message as _;
use tierstore_trees_api::provider::v1::{MountMode, PrepareRequest, Source, SweepRequest};
use tierstore_trees_api::provider::{METHODS, PACKAGE, SERVICE, path};

/// Asserts the consts are what `provider.proto` declares, so that a path built from them is the
/// path the generated server serves.
#[test]
fn names_are_the_ones_of_the_provider_proto() {
    let set = protox::compile(["jobscheduler/provider/v1/provider.proto"], ["proto"])
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
    };
    let bytes = [
        0x0a, 0x01, b'a', 0x12, 0x04, b'j', b's', b'.', b'1', 0x12, 0x04, b'j', b's', b'.', b'2',
    ];
    assert_eq!(sweep.encode_to_vec(), bytes);
}
