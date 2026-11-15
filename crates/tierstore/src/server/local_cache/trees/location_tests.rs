//! The rules of a location, of the table that finds the placement of a key, and of the id of a
//! placement.

use super::location::{Location, LocationError, Meets, PlacementId, Table, meets, under};
use crate::TreeDigest;

fn id(text: &str) -> PlacementId {
    PlacementId(text.to_owned())
}

/// A table with the placements `a` at `names`, each location in the order given.
fn table(locations: &[(&str, Location)]) -> Table {
    locations
        .iter()
        .map(|(name, location)| (location.key(), id(name)))
        .collect()
}

fn s3(bucket: &str, prefix: &str) -> Location {
    Location::s3(bucket, prefix).unwrap()
}

fn fs(path: &str) -> Location {
    Location::fs(path).unwrap()
}

/// Asserts that an S3 location is a bucket with no `/` or NUL and a prefix that ends in `/`, does
/// not begin with one, has no NUL and is at most a key's length, each refused by name.
#[test]
fn an_s3_location_is_a_bucket_and_a_prefix_that_ends_in_a_slash() {
    assert_eq!(
        s3("b", "jobs/1/"),
        Location::S3 {
            bucket: "b".to_owned(),
            prefix: "jobs/1/".to_owned()
        }
    );
    for bucket in ["", "a/b", "a\0b"] {
        assert_eq!(Location::s3(bucket, "p/"), Err(LocationError::Bucket));
    }
    let long = format!("{}/", "a".repeat(1024));
    for prefix in ["", "jobs", "/jobs/", "jo\0bs/", long.as_str()] {
        assert_eq!(Location::s3("b", prefix), Err(LocationError::Prefix));
    }
    let longest = format!("{}/", "a".repeat(1023));
    assert!(Location::s3("b", &longest).is_ok());
}

/// Asserts that a path of the mount is names of the usual rules joined by `/`, that none of the
/// mount's own directories (`trees`, `views`, `put`) is first, and that one may be below another
/// name.
#[test]
fn an_fs_location_is_names_and_never_a_directory_of_the_mount() {
    assert_eq!(
        fs("jobs/1f3a/root"),
        Location::Fs {
            path: "jobs/1f3a/root".to_owned()
        }
    );
    assert!(Location::fs("jobs/trees").is_ok());
    for path in [
        "", "/a", "a/", "a//b", "a/./b", "a/../b", "a/.wh.x", "a\0", "trees", "views/x", "put",
    ] {
        assert_eq!(Location::fs(path), Err(LocationError::Path), "{path:?}");
    }
}

/// Asserts that a location is shown as its interface and place, and that the refusals say what is
/// wrong.
#[test]
fn locations_and_their_refusals_are_shown() {
    assert_eq!(s3("b", "p/").to_string(), "s3 b/p/");
    assert_eq!(fs("a/b").to_string(), "fs a/b");
    assert!(LocationError::Bucket.to_string().contains("bucket"));
    assert!(LocationError::Prefix.to_string().contains("prefix"));
    assert!(LocationError::Path.to_string().contains("path"));
}

/// Asserts that a placement's id is the same for the same tree and the same locations in any
/// order, differs with the tree, with a location and with the interface, and is thirty-two hex
/// digits that `parse` takes back.
#[test]
fn the_id_of_a_placement_is_of_its_tree_and_its_locations_in_any_order() {
    let tree = TreeDigest::of(b"a");
    let (one, two) = (s3("b", "x/"), fs("x"));
    let both = PlacementId::of(&tree, &[one.clone(), two.clone()]);
    assert_eq!(both, PlacementId::of(&tree, &[two.clone(), one.clone()]));
    assert_ne!(
        both,
        PlacementId::of(&TreeDigest::of(b"b"), &[one.clone(), two])
    );
    assert_ne!(both, PlacementId::of(&tree, std::slice::from_ref(&one)));
    assert_ne!(
        PlacementId::of(&tree, &[s3("b", "x/")]),
        PlacementId::of(&tree, &[s3("c", "x/")])
    );
    assert_eq!(both.as_str().len(), 32);
    assert_eq!(PlacementId::parse(both.as_str()), Some(both.clone()));
    assert_eq!(both.to_string(), both.as_str());
    for text in ["", "abc", &"A".repeat(32), &"g".repeat(32), &"a".repeat(33)] {
        assert_eq!(PlacementId::parse(text), None, "{text:?}");
    }
}

/// Asserts which placement a new location meets: its own place (equal), a place it is inside or
/// around (overlaps), and none for a place that only shares a name's first letters, another
/// bucket, or the other interface.
#[test]
fn a_new_location_meets_the_placement_it_is_equal_to_inside_or_around() {
    let held = table(&[
        ("one", s3("b", "jobs/1/")),
        ("two", fs("jobs/2")),
        ("three", s3("b", "deep/a/b/")),
    ]);
    let key = |location: Location| location.key();
    assert_eq!(
        meets(&held, &key(s3("b", "jobs/1/"))),
        Some(Meets::Equal(id("one")))
    );
    assert_eq!(
        meets(&held, &key(fs("jobs/2"))),
        Some(Meets::Equal(id("two")))
    );
    // Inside a placement.
    assert_eq!(
        meets(&held, &key(s3("b", "jobs/1/x/"))),
        Some(Meets::Overlaps(id("one")))
    );
    assert_eq!(
        meets(&held, &key(fs("jobs/2/x/y"))),
        Some(Meets::Overlaps(id("two")))
    );
    // Around a placement.
    assert_eq!(
        meets(&held, &key(s3("b", "jobs/"))),
        Some(Meets::Overlaps(id("one")))
    );
    assert_eq!(
        meets(&held, &key(s3("b", "deep/"))),
        Some(Meets::Overlaps(id("three")))
    );
    assert_eq!(
        meets(&held, &key(fs("jobs"))),
        Some(Meets::Overlaps(id("two")))
    );
    // Not a placement's place: a longer name, another bucket, the other interface.
    assert_eq!(meets(&held, &key(s3("b", "jobs/10/"))), None);
    assert_eq!(meets(&held, &key(s3("c", "jobs/1/"))), None);
    assert_eq!(meets(&held, &key(fs("jobs/1"))), None);
    assert_eq!(meets(&held, &key(s3("b", "other/"))), None);
    assert_eq!(meets(&Table::new(), &key(fs("a"))), None);
}

/// Asserts that a key finds the placement at any `/` boundary above it and gives the rest, that
/// the key that is the prefix itself gives an empty rest, and that a key that only begins with the
/// prefix's letters, one in another bucket or under none finds nothing.
#[test]
fn a_key_finds_the_placement_it_falls_under_and_the_path_below_it() {
    let held = table(&[("one", s3("b", "jobs/1/root/")), ("two", fs("a/b"))]);
    let s3_scope = "s3\0b\0";
    let found = |scope: &str, rest: &str| {
        under(&held, scope, rest).map(|(id, rest)| (id.0, rest.to_owned()))
    };
    assert_eq!(
        found(s3_scope, "jobs/1/root/bin/cc"),
        Some(("one".to_owned(), "bin/cc".to_owned()))
    );
    assert_eq!(
        found(s3_scope, "jobs/1/root/"),
        Some(("one".to_owned(), String::new()))
    );
    assert_eq!(found(s3_scope, "jobs/1/root"), None);
    assert_eq!(found(s3_scope, "jobs/1/rootx/a"), None);
    assert_eq!(found(s3_scope, "jobs/1/"), None);
    assert_eq!(found("s3\0c\0", "jobs/1/root/a"), None);
    assert_eq!(
        found("fs\0\0", "a/b/c/"),
        Some(("two".to_owned(), "c/".to_owned()))
    );
}
