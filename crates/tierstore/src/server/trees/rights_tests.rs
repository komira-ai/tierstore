//! What a key may ask of the interface, from the request alone.

use tierstore_trees_api::v1::{
    ChildTree, CreateTreeRequest, Dir, Entry, File, FsLocation, Location, S3Location, Source,
    Symlink, entry, location,
};
use tonic::{Code, Status};

use super::Caller;
use super::rig::{FULL, READER, SCOPED, assert_refused, config};
use super::rights::{permit_create, permit_delete, permit_placement};
use crate::server::keys::Role;

fn caller(key: (&str, &str)) -> Caller {
    let config = config();
    let (_, role) = config.key(key.0).expect("a key of the server");
    Caller {
        key_id: key.0.to_owned(),
        role,
    }
}

fn presigning() -> Caller {
    Caller {
        key_id: "presign".to_owned(),
        role: Role::PresignPut,
    }
}

fn file(path: &str, sha256: Vec<u8>, from: Option<&str>) -> Entry {
    let from = from.map(|key| Source {
        key: key.to_owned(),
        offset: 0,
    });
    Entry {
        path: path.to_owned(),
        kind: Some(entry::Kind::File(File {
            mode: None,
            sha256,
            size: 1,
            from,
        })),
    }
}

fn s3(prefix: &str) -> Location {
    Location {
        r#where: Some(location::Where::S3(S3Location {
            bucket: "tier-bucket".to_owned(),
            prefix: prefix.to_owned(),
        })),
    }
}

fn fs(path: &str) -> Location {
    Location {
        r#where: Some(location::Where::Fs(FsLocation {
            path: path.to_owned(),
        })),
    }
}

fn create(entries: Vec<Entry>, at: Vec<Location>) -> CreateTreeRequest {
    CreateTreeRequest {
        entries,
        at,
        ..CreateTreeRequest::default()
    }
}

/// Asserts the full key may create a tree from any source at any location, with no look at the
/// request: whether the request is right is the core's to judge.
#[test]
fn the_full_key_may_create_anywhere() {
    let request = create(
        vec![file("a", vec![1], Some("_journal/x"))],
        vec![fs("jobs/1"), s3("any/")],
    );
    assert_eq!(permit_create(&caller(FULL), &request), Ok(()));
}

/// Asserts a read-only key and a presigning key may not create or release, with the message the S3
/// front gives a read-only key, and that every key but a presigning one may ask for a placement.
#[test]
fn a_key_that_does_not_write_may_not_create_or_release() {
    let request = create(vec![], vec![]);
    let message = "Access Denied: this access key may read objects and list the bucket only.";
    for who in [caller(READER), presigning()] {
        let refused = permit_create(&who, &request).expect_err("refused");
        assert_refused(
            &Status::from(refused),
            Code::PermissionDenied,
            &format!("access denied: {message}"),
        );
        let refused = permit_delete(&who).expect_err("refused");
        assert_refused(
            &Status::from(refused),
            Code::PermissionDenied,
            &format!("access denied: {message}"),
        );
    }
    assert_eq!(permit_delete(&caller(FULL)), Ok(()));
    assert_eq!(permit_delete(&caller(SCOPED)), Ok(()));
    for who in [caller(FULL), caller(READER), caller(SCOPED)] {
        assert_eq!(permit_placement(&who), Ok(()));
    }
    let refused = permit_placement(&presigning()).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: this key may not read",
    );
}

/// Asserts a scoped key may create when every source is under its read prefixes and every `s3`
/// location is under its write prefixes, whichever way a source is named, and that entries that
/// have no source, and a location that names none, are not judged.
#[test]
fn a_scoped_key_may_create_what_its_prefixes_cover() {
    let whole = vec![0xab; 32];
    let entries = vec![
        file("bin/cc", vec![1], Some("cas/d/n7/b2/0000000000000041")),
        file("lib/libc.a", whole, None),
        Entry {
            path: "include".to_owned(),
            kind: Some(entry::Kind::Dir(Dir { mode: None })),
        },
        Entry {
            path: "lib64".to_owned(),
            kind: Some(entry::Kind::Symlink(Symlink {
                target: "lib".to_owned(),
            })),
        },
        Entry {
            path: "nothing".to_owned(),
            kind: None,
        },
    ];
    let at = vec![s3("jobs/1f3a/root/"), Location { r#where: None }];
    assert_eq!(permit_create(&caller(SCOPED), &create(entries, at)), Ok(()));
}

/// Asserts a scoped key is refused naming the first source it may not read: one outside its read
/// prefixes, a reserved one, and the whole object of a file with no `from`, whose key is made from
/// the digest.
#[test]
fn a_scoped_key_may_not_read_what_its_prefixes_do_not_cover() {
    let at = vec![s3("jobs/1/")];
    let outside = create(vec![file("a", vec![1], Some("other/x"))], at.clone());
    let refused = permit_create(&caller(SCOPED), &outside).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: this key may not read other/x",
    );

    let reserved = create(vec![file("a", vec![1], Some("_journal/x"))], at.clone());
    let refused = permit_create(&caller(SCOPED), &reserved).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: this key may not read _journal/x",
    );

    // `cas/l/<hex>` is under `cas/`: a whole object of a digest is readable; a first source that
    // is not is the one named, and an entry after it is not judged.
    let digest = vec![0x0f; 32];
    let first = file("a", digest, None);
    let second = file("b", vec![1], Some("secret/y"));
    let third = file("c", vec![1], Some("secret/z"));
    let request = create(vec![first, second, third], at);
    let refused = permit_create(&caller(SCOPED), &request).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: this key may not read secret/y",
    );
}

/// Asserts a file with no `from` and a digest that is not 32 bytes is refused as a malformed digest
/// at its path, since it names no object, and that this is judged after the entries before it.
#[test]
fn a_whole_object_with_a_short_digest_names_no_object() {
    let request = create(
        vec![file("bin/cc", vec![1, 2, 3], None)],
        vec![s3("jobs/1/")],
    );
    let refused = permit_create(&caller(SCOPED), &request).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::InvalidArgument,
        "malformed digest: bin/cc",
    );
}

/// Asserts a scoped key is refused a location outside its write prefixes, naming it, and an `fs`
/// location whatever its prefixes are: only the full key places a tree at a path of the mount.
#[test]
fn a_scoped_key_may_not_write_outside_its_prefixes_or_to_a_path() {
    let source = vec![file("a", vec![1], Some("cas/x/1"))];
    let outside = create(source.clone(), vec![s3("jobs/1/"), s3("builds/2/")]);
    let refused = permit_create(&caller(SCOPED), &outside).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: this key may not write builds/2/",
    );

    let path = create(source, vec![fs("jobs/1/root")]);
    let refused = permit_create(&caller(SCOPED), &path).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: only the full key may place a tree at the path jobs/1/root",
    );
}

/// Asserts a tree entry (a tree on the node, by digest) is refused to a scoped key, naming its
/// path, after the files before it are judged, and that the full key may place one.
#[test]
fn a_scoped_key_may_not_place_a_tree_by_digest() {
    let child = Entry {
        path: "toolchain".to_owned(),
        kind: Some(entry::Kind::Tree(ChildTree { tree: vec![0; 32] })),
    };
    let request = create(
        vec![file("a", vec![1], Some("cas/x/1")), child.clone()],
        vec![s3("jobs/1/")],
    );
    let refused = permit_create(&caller(SCOPED), &request).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: only the full key may place a tree by digest at toolchain",
    );
    assert_eq!(permit_create(&caller(FULL), &request), Ok(()));

    let unreadable = create(
        vec![file("a", vec![1], Some("secret/y")), child],
        vec![s3("jobs/1/")],
    );
    let refused = permit_create(&caller(SCOPED), &unreadable).expect_err("refused");
    assert_refused(
        &Status::from(refused),
        Code::PermissionDenied,
        "access denied: this key may not read secret/y",
    );
}

/// Asserts an S3 location under a write prefix of a scoped key is refused when it is not a plain
/// name: a `..` or `.` segment that leaves the prefix, or a segment named like the proxy's.
#[test]
fn a_scoped_key_may_not_write_through_a_dot_segment_or_to_a_reserved_prefix() {
    let source = vec![file("a", vec![1], Some("cas/x/1"))];
    for prefix in [
        "jobs/../_journal/",
        "jobs/1/../../builds/",
        "jobs/./1/",
        "_journal/1/",
    ] {
        let request = create(source.clone(), vec![s3(prefix)]);
        let refused = permit_create(&caller(SCOPED), &request).expect_err(prefix);
        assert_refused(
            &Status::from(refused),
            Code::PermissionDenied,
            &format!("access denied: this key may not write {prefix}"),
        );
    }
    let fine = create(source, vec![s3("jobs/1/..a/")]);
    assert_eq!(permit_create(&caller(SCOPED), &fine), Ok(()));
}
