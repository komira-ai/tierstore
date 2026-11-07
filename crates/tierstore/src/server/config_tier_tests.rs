//! The marks bucket of the hot tier, as a file names it or leaves it out: the tier it makes for each
//! kind of hot tier, what it refuses, and that the second handle a start opens for it is a store of
//! its own.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;

use super::marks_directory;
use crate::Error;
use crate::server::cli::{marks_name, open_marks};
use crate::server::config::{from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, GOLDEN, MINIMAL};

/// What a start of `text` is refused with.
fn refusal(text: &str) -> String {
    settings_from_text(text, DEFAULTS).unwrap_err().to_string()
}

/// The tier of the markers that the storage node's file `text` makes.
fn marks_of(text: &str) -> String {
    from_text(text, DEFAULTS).unwrap().hot_marks
}

/// `MINIMAL` with its hot tier replaced by `hot` (the lines of the table).
fn with_hot(hot: &str) -> String {
    MINIMAL.replacen("kind   = \"memory\"\nbudget = 1000", hot, 1)
}

/// Asserts that an S3 hot tier with no `marks_bucket` keeps its markers in `<bucket>-marks`, and
/// one that names a bucket keeps them there, with the golden file's other keys as they were.
#[test]
fn an_s3_hot_tier_keeps_its_markers_in_the_bucket_with_marks_added_or_the_one_named() {
    assert_eq!(marks_of(GOLDEN), "s3:tierstore-marks");
    let default = GOLDEN.replacen("marks_bucket       = \"tierstore-marks\"\n", "", 1);
    assert_eq!(marks_of(&default), "s3:tierstore-hot-marks");
    let named = GOLDEN.replacen("\"tierstore-marks\"", "\"tier-markers\"", 1);
    let cli = from_text(&named, DEFAULTS).unwrap();
    assert_eq!(cli.hot_marks, "s3:tier-markers");
    assert_eq!(cli.hot, "s3:tierstore-hot");
}

/// Asserts that a `dir` hot tier keeps its markers in the directory of its path with `-marks` added,
/// beside it, whether the path ends with a separator or has a `.` in it, and a `memory` one in a
/// store of its own.
#[test]
fn a_dir_hot_tier_keeps_its_markers_beside_it_and_a_memory_one_in_a_map_of_its_own() {
    for (path, marks) in [
        ("/var/hot", "dir:/var/hot-marks"),
        ("/var/hot/", "dir:/var/hot-marks"),
        ("/var/./hot//", "dir:/var/hot-marks"),
        ("hot", "dir:hot-marks"),
    ] {
        let text = with_hot(&format!("kind = \"dir\"\npath = \"{path}\"\nbudget = 1000"));
        assert_eq!(marks_of(&text), marks, "{path}");
    }
    assert_eq!(marks_of(MINIMAL), "memory");
    assert_eq!(marks_directory(Path::new("/")), Path::new("/-marks"));
}

/// Asserts the refusals of `marks_bucket`, each naming the key, with the message exactly: for a
/// `memory` and for a `dir` hot tier (it is for `s3` only), for a bucket that is the hot tier's own,
/// for an empty name, and in `[cold_store]`.
#[test]
fn a_marks_bucket_is_refused_where_it_means_nothing_or_names_the_hot_bucket() {
    let only_s3 = "hot_store.marks_bucket: only for kind \"s3\": a dir tier keeps its markers in \
                   the directory of its path with \"-marks\" added, and a memory tier in a map of \
                   its own";
    let memory = with_hot("kind = \"memory\"\nbudget = 1000\nmarks_bucket = \"m\"");
    assert_eq!(refusal(&memory), only_s3);
    let dir = with_hot("kind = \"dir\"\npath = \"/var/hot\"\nbudget = 1000\nmarks_bucket = \"m\"");
    assert_eq!(refusal(&dir), only_s3);

    let same = GOLDEN.replacen("\"tierstore-marks\"", "\"tierstore-hot\"", 1);
    assert_eq!(
        refusal(&same),
        "hot_store.marks_bucket: is the bucket of the tier, \"tierstore-hot\"; the markers need a \
         bucket of their own"
    );
    let empty = GOLDEN.replacen("\"tierstore-marks\"", "\"\"", 1);
    assert_eq!(refusal(&empty), "hot_store.marks_bucket: is empty");

    let cold = GOLDEN.replacen(
        "[cold_store]\n",
        "[cold_store]\nmarks_bucket = \"c-marks\"\n",
        1,
    );
    assert_eq!(
        refusal(&cold),
        "cold_store.marks_bucket: only [hot_store] has it"
    );
}

/// Asserts that the second handle a start opens is a store of its own for a `memory` hot tier (an
/// object put through one open is not in the store of another, so it cannot be the hot tier's), is made for an
/// S3 hot tier without asking the store anything (the endpoint is a port nothing listens on), and is
/// none for a `dir` hot tier, whose directory is not made: nothing is made beside the hot one.
#[tokio::test]
async fn the_second_handle_is_a_store_of_its_own_and_a_dir_hot_tier_makes_no_directory() {
    let timeout = Duration::from_secs(30);
    let mut cli = from_text(MINIMAL, DEFAULTS).unwrap();
    let marks = open_marks(&cli, timeout).unwrap().unwrap();
    marks.put("k", Bytes::from_static(b"marker")).await.unwrap();
    let again = open_marks(&cli, timeout).unwrap().unwrap();
    assert_eq!(again.head("k").await, Err(Error::NotFound("k".to_owned())));

    let scratch = TestDir::under(std::env::temp_dir(), "tierstore-marks-handle");
    let hot_dir = scratch.path().join("hot");
    cli.hot = format!("dir:{}", hot_dir.display());
    cli.hot_marks = format!("dir:{}-marks", hot_dir.display());
    assert!(open_marks(&cli, timeout).unwrap().is_none());
    assert!(!scratch.path().join("hot-marks").exists());
    assert!(!hot_dir.exists());

    let credentials = scratch.path().join("credentials");
    std::fs::write(&credentials, "test-key\ntest-secret\n").unwrap();
    std::fs::set_permissions(
        &credentials,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    cli.hot = "s3:tier-hot".to_owned();
    cli.hot_marks = "s3:tier-hot-marks".to_owned();
    cli.hot_endpoint = Some("http://127.0.0.1:9".to_owned());
    cli.hot_credentials_file = Some(credentials);
    assert!(open_marks(&cli, timeout).unwrap().is_some());

    cli.hot_credentials_file = None;
    let refused = open_marks(&cli, timeout).err().unwrap();
    assert_eq!(
        refused,
        "--hot \"s3:tier-hot-marks\": it needs --hot-credentials-file"
    );
}

/// Asserts the name a proxy compares with the format record: the bucket of an S3 marks tier, the
/// word `memory` for a memory tier's, and none for a `dir` tier, which has no handle.
#[test]
fn the_marks_bucket_a_proxy_names_is_the_s3_bucket_or_memory_and_a_dir_tier_has_none() {
    assert_eq!(
        marks_name("s3:tier-hot-marks"),
        Some("tier-hot-marks".to_owned())
    );
    assert_eq!(marks_name("memory"), Some("memory".to_owned()));
    assert_eq!(marks_name("dir:/srv/hot-marks"), None);
}

/// Asserts that `endpoints` of an `s3` tier, hot or cold, is the endpoint list in order (the first
/// is `endpoint`, the others follow it), and that a list of one makes the same settings as
/// `endpoint` does.
#[test]
fn a_list_of_endpoints_is_the_s3_tiers_endpoints_in_order() {
    let one = from_text(GOLDEN, DEFAULTS).unwrap();
    assert_eq!(one.hot_more_endpoints, Vec::<String>::new());
    assert_eq!(one.cold_more_endpoints, Vec::<String>::new());
    let listed = GOLDEN
        .replacen(
            "endpoint           = \"http://192.0.2.10:9200\"",
            "endpoints          = [\"http://192.0.2.10:9200\"]",
            1,
        )
        .replacen(
            "endpoint         = \"http://127.0.0.1:30900\"",
            "endpoints        = [\"http://127.0.0.1:30900\"]",
            1,
        );
    assert_ne!(listed, GOLDEN);
    assert_eq!(from_text(&listed, DEFAULTS).unwrap(), one);

    let several = GOLDEN
        .replacen(
            "endpoint           = \"http://192.0.2.10:9200\"",
            "endpoints          = [\"http://192.0.2.11:9200\", \"http://192.0.2.12:9200\", \"http://192.0.2.13:9200\"]",
            1,
        )
        .replacen(
            "endpoint         = \"http://127.0.0.1:30900\"",
            "endpoints        = [\"http://127.0.0.1:30901\", \"http://127.0.0.1:30902\"]",
            1,
        );
    let cli = from_text(&several, DEFAULTS).unwrap();
    assert_eq!(cli.hot_endpoint.as_deref(), Some("http://192.0.2.11:9200"));
    assert_eq!(
        cli.hot_more_endpoints,
        ["http://192.0.2.12:9200", "http://192.0.2.13:9200"]
    );
    assert_eq!(cli.cold_endpoint.as_deref(), Some("http://127.0.0.1:30901"));
    assert_eq!(cli.cold_more_endpoints, ["http://127.0.0.1:30902"]);
}

/// Asserts the refusals of the endpoints of an `s3` tier, each naming the key: both `endpoint` and
/// `endpoints`, an empty list, an endpoint twice, neither, and a list in a tier that is not `s3`.
#[test]
fn the_endpoints_of_an_s3_tier_are_refused_when_both_empty_twice_or_missing() {
    let hot = "endpoint           = \"http://192.0.2.10:9200\"\n";
    let cold = "endpoint         = \"http://127.0.0.1:30900\"\n";
    for (table, line, name) in [("hot", hot, "hot_store"), ("cold", cold, "cold_store")] {
        let with = |replacement: &str| GOLDEN.replacen(line, replacement, 1);
        let both = with(&format!("{line}endpoints = [\"http://x:1\"]\n"));
        let empty = with("endpoints = []\n");
        let twice = with("endpoints = [\"http://x:1\", \"http://y:1\", \"http://x:1\"]\n");
        let neither = with("");
        let key = format!("{name}.endpoints");
        for (text, why) in [
            (both, format!("{key}: give endpoint or endpoints, not both")),
            (
                empty,
                format!("{key}: is empty; give at least one endpoint"),
            ),
            (twice, format!("{key}: \"http://x:1\" is given twice")),
            (neither, format!("{name}.endpoint: it is needed")),
        ] {
            assert_eq!(refusal(&text), why, "{table}");
        }
    }
    let dir = with_hot(
        "kind = \"dir\"\npath = \"/var/hot\"\nendpoints = [\"http://x:1\"]\nbudget = 1000",
    );
    assert_eq!(refusal(&dir), "hot_store.endpoints: not for kind \"dir\"");
    let memory = with_hot("kind = \"memory\"\nendpoints = [\"http://x:1\"]\nbudget = 1000");
    assert_eq!(
        refusal(&memory),
        "hot_store.endpoints: not for kind \"memory\""
    );
}
