//! The wire contract is only extended (AGENTS.md B8): the descriptor of the released
//! `trees.proto` is checked in as a golden, and the current proto must keep every message, field
//! number and type, `oneof` member, enum value, service and method of it.

use std::collections::BTreeMap;

use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet};
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, Entry, File, GetPlacementResponse, Prefetch, entry,
};

/// The descriptor of the released proto.
const GOLDEN: &[u8] = include_bytes!("golden/trees.v1.fds");

/// The descriptor of the proto of this tree, compiled as `build.rs` compiles it.
fn current() -> FileDescriptorSet {
    protox::compile(["tierstore/trees/v1/trees.proto"], ["proto"]).expect("the proto compiles")
}

fn golden() -> FileDescriptorSet {
    FileDescriptorSet::decode(GOLDEN).expect("the golden descriptor decodes")
}

/// Every message of `set` by its full name, nested messages included.
fn messages(set: &FileDescriptorSet) -> BTreeMap<String, &DescriptorProto> {
    fn walk<'a>(
        prefix: &str,
        message: &'a DescriptorProto,
        out: &mut BTreeMap<String, &'a DescriptorProto>,
    ) {
        let name = format!("{prefix}.{}", message.name());
        for nested in &message.nested_type {
            walk(&name, nested, out);
        }
        out.insert(name, message);
    }
    let mut out = BTreeMap::new();
    for file in &set.file {
        for message in &file.message_type {
            walk(&format!(".{}", file.package()), message, &mut out);
        }
    }
    out
}

fn enums(set: &FileDescriptorSet) -> BTreeMap<String, &EnumDescriptorProto> {
    let mut out = BTreeMap::new();
    for file in &set.file {
        for item in &file.enum_type {
            out.insert(format!(".{}.{}", file.package(), item.name()), item);
        }
    }
    out
}

/// Everything of `golden` that `current` lacks or has changed, one line each.
fn broken(golden: &FileDescriptorSet, current: &FileDescriptorSet) -> Vec<String> {
    let mut problems = Vec::new();
    let now = messages(current);
    for (name, old) in messages(golden) {
        let Some(new) = now.get(&name) else {
            problems.push(format!("message {name} is gone"));
            continue;
        };
        for field in &old.field {
            let found = new.field.iter().find(|f| f.number() == field.number());
            let Some(found) = found else {
                problems.push(format!(
                    "{name}.{} (field {}) is gone",
                    field.name(),
                    field.number()
                ));
                continue;
            };
            let same = found.name() == field.name()
                && found.r#type() == field.r#type()
                && found.label() == field.label()
                && found.type_name() == field.type_name()
                && found.proto3_optional() == field.proto3_optional()
                && found.oneof_index.map(|i| new.oneof_decl[i as usize].name())
                    == field.oneof_index.map(|i| old.oneof_decl[i as usize].name());
            if !same {
                problems.push(format!(
                    "{name}.{} (field {}) changed",
                    field.name(),
                    field.number()
                ));
            }
        }
    }
    let now = enums(current);
    for (name, old) in enums(golden) {
        let Some(new) = now.get(&name) else {
            problems.push(format!("enum {name} is gone"));
            continue;
        };
        for value in &old.value {
            let kept = new
                .value
                .iter()
                .any(|v| v.number() == value.number() && v.name() == value.name());
            if !kept {
                problems.push(format!(
                    "{name}.{} ({}) is gone",
                    value.name(),
                    value.number()
                ));
            }
        }
    }
    let services: BTreeMap<_, _> = current
        .file
        .iter()
        .flat_map(|file| &file.service)
        .map(|service| (service.name(), service))
        .collect();
    for service in golden.file.iter().flat_map(|file| &file.service) {
        let Some(new) = services.get(service.name()) else {
            problems.push(format!("service {} is gone", service.name()));
            continue;
        };
        for method in &service.method {
            let kept = new.method.iter().any(|m| {
                m.name() == method.name()
                    && m.input_type() == method.input_type()
                    && m.output_type() == method.output_type()
                    && m.client_streaming() == method.client_streaming()
                    && m.server_streaming() == method.server_streaming()
            });
            if !kept {
                problems.push(format!(
                    "method {}.{} is gone or changed",
                    service.name(),
                    method.name()
                ));
            }
        }
    }
    let package = |set: &FileDescriptorSet| set.file.first().map(|f| f.package().to_owned());
    if package(golden) != package(current) {
        problems.push("the package changed".to_owned());
    }
    problems
}

/// The message `name` of `set`, mutable.
fn message_mut<'a>(set: &'a mut FileDescriptorSet, name: &str) -> &'a mut DescriptorProto {
    set.file[0]
        .message_type
        .iter_mut()
        .find(|m| m.name() == name)
        .expect("the message exists")
}

#[test]
fn trees_proto_is_additive() {
    assert_eq!(broken(&golden(), &current()), Vec::<String>::new());
}

#[test]
fn additivity_check_fails_on_a_removed_renumbered_or_retyped_member() {
    let released = golden();

    let mut removed = current();
    message_mut(&mut removed, "CreateTreeRequest").field.pop();
    assert_eq!(
        broken(&released, &removed),
        [".tierstore.trees.v1.CreateTreeRequest.tree (field 6) is gone"]
    );

    let mut renumbered = current();
    message_mut(&mut renumbered, "Failure").field[1].number = Some(9);
    assert_eq!(
        broken(&released, &renumbered),
        [".tierstore.trees.v1.Failure.code (field 2) is gone"]
    );

    let mut retyped = current();
    message_mut(&mut retyped, "File").field[2].r#type = Some(Type::Uint32 as i32);
    assert_eq!(
        broken(&released, &retyped),
        [".tierstore.trees.v1.File.size (field 3) changed"]
    );

    let mut relabelled = current();
    message_mut(&mut relabelled, "Entry").field[0].label = Some(Label::Repeated as i32);
    assert_eq!(
        broken(&released, &relabelled),
        [".tierstore.trees.v1.Entry.path (field 1) changed"]
    );

    let mut dropped = current();
    dropped.file[0].message_type.retain(|m| m.name() != "Dir");
    dropped.file[0].enum_type.retain(|e| e.name() != "Prefetch");
    dropped.file[0].service.clear();
    let problems = broken(&released, &dropped);
    assert_eq!(
        problems,
        [
            "message .tierstore.trees.v1.Dir is gone",
            "enum .tierstore.trees.v1.Prefetch is gone",
            "service TreesService is gone"
        ]
    );

    let mut value = current();
    value.file[0].enum_type[0].value.pop();
    assert_eq!(
        broken(&released, &value),
        [".tierstore.trees.v1.Prefetch.PREFETCH_WAIT (3) is gone"]
    );

    let mut method = current();
    method.file[0].service[0].method[1].output_type = Some(".tierstore.trees.v1.Failure".into());
    assert_eq!(
        broken(&released, &method),
        ["method TreesService.GetPlacement is gone or changed"]
    );

    let mut package = current();
    package.file[0].package = Some("tierstore.trees.v2".into());
    assert!(broken(&released, &package).contains(&"the package changed".to_owned()));
}

/// A request and a response, encoded by the release this golden is of.
const REQUEST_V1: &[u8] = &[
    0x0a, 0x0c, 0x0a, 0x05, b'b', b'i', b'n', b'/', b'c', 0x12, 0x03, 0x08, 0xed, 0x03, 0x18, 0x03,
    0x20, 0x3c,
];

#[test]
fn trees_proto_golden_bytes_decode_both_ways() {
    // The bytes of the released request decode with the current types.
    let request = CreateTreeRequest::decode(REQUEST_V1).expect("the released bytes decode");
    assert_eq!(request.prefetch, Prefetch::Wait as i32);
    assert_eq!(request.ttl_secs, Some(60));
    let entry = &request.entries[0];
    assert_eq!(entry.path, "bin/c");
    // `file { mode: 493 }` is a `File` whose mode is the only member.
    assert_eq!(
        entry.kind,
        Some(entry::Kind::File(File {
            mode: Some(0o755),
            ..File::default()
        }))
    );
    // The current types write the bytes the release wrote.
    assert_eq!(request.encode_to_vec(), REQUEST_V1);

    // A message with a field the golden types do not have (field 99, a varint) is read by them,
    // the unknown field skipped, and the members they know are as sent.
    let mut newer = REQUEST_V1.to_vec();
    newer.extend_from_slice(&[0x98, 0x06, 0x07]);
    let read = CreateTreeRequest::decode(newer.as_slice()).expect("an added field is skipped");
    assert_eq!(read, request);

    // The same for the response, and for a message of one released type inside another.
    let response = CreateTreeResponse {
        files: 2,
        created: true,
        ..CreateTreeResponse::default()
    };
    let mut bytes = response.encode_to_vec();
    bytes.extend_from_slice(&[0xa0, 0x06, 0x01]);
    assert_eq!(CreateTreeResponse::decode(bytes.as_slice()), Ok(response));
    assert!(GetPlacementResponse::decode([].as_slice()).is_ok());
    assert_eq!(Entry::default().kind, None);
}
