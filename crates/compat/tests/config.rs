//! The configuration file across the release before this one and this branch: each release's own
//! entry point (`server::cli::main`, which the binary calls) is run on the same file with
//! `--check-config`, which loads the file as a start does and starts nothing. The release before
//! is the real `tierstore` library of its commit (`tierstore_prev`), not a copy of its loader.

use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

/// A scratch directory with the key files a storage-node file names, and the file's text made of
/// them: two tiers (the hot one an S3 bucket, the cold one in memory) with `hot_extra` in
/// `[hot_store]`, `cold_extra` in `[cold_store]` and `server_extra` in `[server]`.
struct Node {
    dir: TestDir,
    key: String,
    credentials: String,
}

impl Node {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let mut paths = Vec::new();
        for file in ["access.key", "hot.credentials"] {
            let path = dir.path().join(file);
            std::fs::write(&path, "AKID\nsecret\n").unwrap();
            std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
            paths.push(path.display().to_string());
        }
        let credentials = paths.pop().unwrap();
        let key = paths.pop().unwrap();
        Self {
            dir,
            key,
            credentials,
        }
    }

    /// The file, written, and its path.
    fn file(&self, server_extra: &str, hot_extra: &str, cold_extra: &str) -> OsString {
        let text = format!(
            "[server]\nlisten = \"127.0.0.1:0\"\nproxy_id = \"p1\"\nbucket = \"tier-bucket\"\n\
             access_key_file = \"{key}\"\n{server_extra}\n\n\
             [hot_store]\nkind = \"s3\"\nbucket = \"hot\"\nendpoint = \"http://127.0.0.1:1\"\n\
             credentials_file = \"{credentials}\"\nbudget = 1000\n{hot_extra}\n\n\
             [cold_store]\nkind = \"memory\"\n{cold_extra}\n",
            key = self.key,
            credentials = self.credentials,
        );
        let path = self.dir.path().join("tierstore.toml");
        std::fs::write(&path, text).unwrap();
        path.into_os_string()
    }
}

/// The arguments of `tierstore --config <file> --check-config`.
fn checking(file: &OsString) -> Vec<OsString> {
    vec![
        OsString::from("tierstore"),
        OsString::from("--config"),
        file.clone(),
        OsString::from("--check-config"),
    ]
}

/// An exit status as text, since an `ExitCode` has no accessor.
fn status(code: ExitCode) -> String {
    format!("{code:?}")
}

/// The status each release exits with when it checks `file`: this branch's, then the release
/// before's.
fn both(file: &OsString) -> (String, String) {
    let now = tierstore::server::cli::main(checking(file));
    let before = tierstore_prev::server::cli::main(checking(file));
    (status(now), status(before))
}

/// Asserts that a file with none of the new key and none of the removed ones, as this release
/// writes it, is loaded by both releases: this one reads what the release before read, and the
/// release before reads what this one writes when no tier sets `conditional_delete`.
#[test]
fn a_file_without_the_new_key_loads_in_both_releases() {
    let node = Node::new("compat-config-plain");
    let ok = status(ExitCode::SUCCESS);
    assert_eq!(both(&node.file("", "", "")), (ok.clone(), ok));
}

/// Asserts that a file that sets the new `conditional_delete` key is read by this release and
/// refused by the release before with status 2 (it refuses a key it does not know), so a host that
/// may roll back to the release before leaves the key out until every process has this release;
/// and that the same file without the key is loaded by both.
#[test]
fn the_new_key_is_read_here_and_refused_by_the_release_before() {
    let node = Node::new("compat-config-new-key");
    let ok = status(ExitCode::SUCCESS);
    let refused = status(ExitCode::from(2));
    for table in ["hot", "cold"] {
        let (hot, cold) = match table {
            "hot" => ("conditional_delete = true", ""),
            _ => ("", "conditional_delete = true"),
        };
        // The cold table of this file is a memory tier, which takes `true`; the hot one is S3.
        let file = node.file("", hot, cold);
        assert_eq!(both(&file), (ok.clone(), refused.clone()), "{table}");
    }
    assert_eq!(both(&node.file("", "", "")), (ok.clone(), ok));
}

/// Asserts that a file written for the release before, which sets `backend_check_wait` in
/// `[server]` and `skip_checks` in `[cold_store]`, is loaded by this release as it is by the
/// release before: the two keys are kept, accepted and ignored.
#[test]
fn a_file_with_the_keys_of_the_removed_self_test_loads_in_both_releases() {
    let node = Node::new("compat-config-removed-keys");
    let file = node.file(
        "backend_check_wait = \"1m\"",
        "",
        "skip_checks = [\"multipart-conditional=because\"]",
    );
    let ok = status(ExitCode::SUCCESS);
    assert_eq!(both(&file), (ok.clone(), ok));
}
