//! The configuration file across the release before this one and this branch: each release's own
//! entry point (`server::cli::main`, which the binary calls) is run on the same file with
//! `--check-config`, which loads the file as a start does and starts nothing. The release before
//! is the real `tierstore` library of its commit (`tierstore_prev`), not a copy of its loader.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

/// A key as both releases take it: the secret has 16 bytes, the least this branch asks by default.
const LONG_ENOUGH: &str = "AKID\n0123456789abcdef\n";

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
        Self::with_secret(name, LONG_ENOUGH)
    }

    /// As `new`, with `access` as the text of the access key file.
    fn with_secret(name: &str, access: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let mut paths = Vec::new();
        for (file, text) in [("access.key", access), ("hot.credentials", LONG_ENOUGH)] {
            let path = dir.path().join(file);
            std::fs::write(&path, text).unwrap();
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
             [hot_store]\nkind = \"s3\"\nbucket = \"hot\"\nendpoint = \"http://hot.invalid\"\n\
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
fn check_args(file: &OsString) -> Vec<OsString> {
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
    let now = tierstore::server::cli::main(check_args(file));
    let before = tierstore_prev::server::cli::main(check_args(file));
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

/// Asserts that a file that sets `hot_store.marks_bucket` on an S3 hot tier is read by this release
/// and refused by the release before with status 2 (it refuses a key it does not know), so a host
/// that may roll back leaves the key out until every process has this release; that the same key
/// in `[cold_store]` is refused by both; and that the file without the key is loaded by both.
#[test]
fn the_marks_bucket_key_is_read_here_and_refused_by_the_release_before() {
    let node = Node::new("compat-config-marks-bucket");
    let ok = status(ExitCode::SUCCESS);
    let refused = status(ExitCode::from(2));
    let file = node.file("", "marks_bucket = \"hot-markers\"", "");
    assert_eq!(both(&file), (ok.clone(), refused.clone()));
    let cold = node.file("", "", "marks_bucket = \"cold-markers\"");
    assert_eq!(both(&cold), (refused.clone(), refused));
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

/// A grant key as the release before wants it: an id and a secret of at least 32 bytes.
const GRANT_KEY: &str = "AKIDGRANT\n0123456789abcdef0123456789abcdef0123456789abcdef\n";

/// Asserts that a file with a grant key file and a grant lifetime, which the release before starts
/// (so it is a file of that release), starts on this branch too: the two keys of the removed read
/// grant are still accepted, and ignored, so that an old unit starts. The grant key file is also
/// missing in a second file, which this branch starts because it does not read it, and the release
/// before refuses because it does.
#[test]
fn a_file_written_for_the_release_before_with_read_grant_keys_starts_on_this_branch() {
    let node = Node::new("compat-config-grant");
    let grant = node.dir.path().join("grant.key");
    std::fs::write(&grant, GRANT_KEY).unwrap();
    std::fs::set_permissions(&grant, Permissions::from_mode(0o600)).unwrap();
    let extra = format!(
        "grant_key_files = [\"{}\"]\ngrant_max_lifetime = \"5m\"",
        grant.display()
    );
    let ok = status(ExitCode::SUCCESS);
    assert_eq!(both(&node.file(&extra, "", "")), (ok.clone(), ok.clone()));

    let gone = node.dir.path().join("no-such-grant.key");
    let extra = format!("grant_key_files = [\"{}\"]", gone.display());
    let (now, before) = both(&node.file(&extra, "", ""));
    assert_eq!(now, ok);
    assert_ne!(before, ok);
}

/// Asserts the other direction: a file with no key of the read grant, which is what this branch
/// documents, starts on the release before and on this branch.
#[test]
fn a_file_without_read_grant_keys_starts_on_the_release_before_and_on_this_branch() {
    let node = Node::new("compat-config-no-grant");
    let ok = status(ExitCode::SUCCESS);
    assert_eq!(both(&node.file("", "", "")), (ok.clone(), ok));
}

/// Asserts that a file with the `listen` key of `[server]` and the `[oci]` table, the shape the
/// release before reads, starts on this branch as it does on that release: this branch reads the
/// two as an `s3` and an `oci` interface. A file with an `[oci]` table that has no `listen` is
/// refused by both.
#[test]
fn a_file_with_the_oci_table_of_the_release_before_starts_on_both() {
    let node = Node::new("compat-config-oci");
    let ok = status(ExitCode::SUCCESS);
    let with = node.file(
        "",
        "",
        "\n[oci]\nlisten = \"127.0.0.1:0\"\nprefix = \"images/\"",
    );
    assert_eq!(both(&with), (ok.clone(), ok.clone()));

    let bare = node.file("", "", "\n[oci]\nprefix = \"images/\"");
    let (now, before) = both(&bare);
    assert_ne!(now, ok);
    assert_ne!(before, ok);
}

/// Asserts the change of the default minimum on a file of the release before: a file whose key has
/// a secret of 6 bytes, which the release before starts, is refused on this branch with the
/// minimum at its default, which is the one deliberate break; with `min_secret_bytes = 0` in the
/// file it starts on this branch again. A file with `min_secret_bytes = 6` is only for this
/// branch (the release before does not know the key), and starts on it.
#[test]
fn a_short_secret_of_the_release_before_is_refused_here_until_the_minimum_is_lowered() {
    let node = Node::with_secret("compat-config-short-secret", "AKID\nsecret\n");
    let ok = status(ExitCode::SUCCESS);
    let (now, before) = both(&node.file("", "", ""));
    assert_ne!(now, ok);
    assert_eq!(before, ok);

    let (now, _) = both(&node.file("min_secret_bytes = 0", "", ""));
    assert_eq!(now, ok);
    let (now, _) = both(&node.file("min_secret_bytes = 6", "", ""));
    assert_eq!(now, ok);
}

/// A self-signed certificate that names `tierstore.test`, and its key, written under `dir`; the
/// paths of the two files. The key is private to its owner, as the front requires.
fn certificate(dir: &TestDir) -> (String, String) {
    let made = rcgen::generate_simple_self_signed(vec!["tierstore.test".to_owned()]).unwrap();
    let mut paths = Vec::new();
    for (name, text) in [
        ("front.pem", made.cert.pem()),
        ("front.key", made.signing_key.serialize_pem()),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        paths.push(path.display().to_string());
    }
    let key = paths.pop().unwrap();
    let cert = paths.pop().unwrap();
    (cert, key)
}

/// Asserts that a file with `tls_cert_file` and `tls_key_file`, the keys of the S3 front's TLS, is
/// started by this branch and is refused by the release before, which does not know the keys (its
/// tables refuse an unknown key, as for every key added since): an operator gives the keys after
/// the upgrade. The same file without the two keys starts on both.
#[test]
fn a_file_with_the_s3_tls_keys_starts_on_this_branch_and_a_file_without_them_on_both() {
    let node = Node::new("compat-config-tls");
    let (cert, key) = certificate(&node.dir);
    let ok = status(ExitCode::SUCCESS);
    assert_eq!(both(&node.file("", "", "")), (ok.clone(), ok.clone()));

    let extra = format!("tls_cert_file = \"{cert}\"\ntls_key_file = \"{key}\"");
    let (now, before) = both(&node.file(&extra, "", ""));
    assert_eq!(now, ok, "this branch starts a file with the TLS keys");
    assert_ne!(
        before, ok,
        "the release before refuses a key it does not know"
    );
}

/// Asserts the one key `require_auth` of an `oci` interface: a file with it set starts on this
/// branch, and the release before refuses the file at start (it does not know `[[interface]]`)
/// rather than serve the front anonymously, so a rollback with the key set cannot open a front the
/// file meant to lock. The same file with the key left out starts on this branch, which serves
/// the front anonymously as before.
#[test]
fn an_oci_interface_with_require_auth_starts_on_this_branch_and_is_refused_by_the_release_before() {
    let node = Node::new("compat-config-oci-auth");
    let ok = status(ExitCode::SUCCESS);
    let locked = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"], require_auth = true }",
    );
    let (now, before) = both(&locked);
    assert_eq!(now, ok);
    assert_ne!(before, ok);

    let open = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"] }",
    );
    assert_eq!(status(tierstore::server::cli::main(check_args(&open))), ok);
}

/// Asserts the one key `stage_read` of an `oci` interface: a file with it set starts on this
/// branch, and the release before refuses the file at start (an unknown param, and it does not
/// know `[[interface]]` either); the same file with the key left out starts on this branch with
/// the default size of a read.
#[test]
fn an_oci_interface_with_stage_read_starts_on_this_branch_and_is_refused_by_the_release_before() {
    let node = Node::new("compat-config-oci-stage");
    let ok = status(ExitCode::SUCCESS);
    let sized = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"], stage_read = 4096 }",
    );
    let (now, before) = both(&sized);
    assert_eq!(now, ok);
    assert_ne!(before, ok);

    let plain = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"] }",
    );
    assert_eq!(status(tierstore::server::cli::main(check_args(&plain))), ok);
}

/// Asserts the three size keys of an `oci` interface, `max_manifest_bytes`, `read_chunk_bytes` and
/// `echo_bytes`: a file with them set starts on this branch, and the release before refuses the
/// file at start (an unknown param, and it does not know `[[interface]]` either).
#[test]
fn an_oci_interface_with_size_keys_starts_on_this_branch_and_is_refused_by_the_release_before() {
    let node = Node::new("compat-config-oci-sizes");
    let ok = status(ExitCode::SUCCESS);
    let sized = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"], max_manifest_bytes = 4096, read_chunk_bytes = 4096, echo_bytes = 64 }",
    );
    let (now, before) = both(&sized);
    assert_eq!(now, ok);
    assert_ne!(before, ok);
}

/// Asserts the one param `upstream` of an `oci` interface: a file with it starts on this branch
/// (`--check-config` makes the pull-through client as the start does), and the release before
/// refuses the file at start (it does not know `[[interface]]`) rather than serve a front that
/// never pulls. The same file without the param starts on this branch, which serves the front as
/// before.
#[test]
fn an_oci_interface_with_an_upstream_starts_on_this_branch_and_is_refused_by_the_release_before() {
    let node = Node::new("compat-config-oci-upstream");
    let ok = status(ExitCode::SUCCESS);
    let pulling = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"], upstream = { url = \"http://127.0.0.1:5000\" } }",
    );
    let (now, before) = both(&pulling);
    assert_eq!(now, ok);
    assert_ne!(before, ok);

    let plain = node.file(
        "",
        "",
        "\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"] }",
    );
    assert_eq!(status(tierstore::server::cli::main(check_args(&plain))), ok);
}

/// Asserts that a file that sets the io keys of `[server]` (the header timeout, the listen backlog,
/// the closing door's times, the multipart sizes and the rest) is read by this branch and refused
/// by the pinned release before with status 2, which predates the warning for an unknown key
/// (#141 on main: a release that has it ignores the keys with one warning each), and that the same
/// file without them is loaded by both. So a host that may roll back to the pinned release leaves
/// the keys out until every process has this branch.
#[test]
fn the_io_keys_are_read_here_and_refused_by_the_pinned_release_before() {
    let node = Node::new("compat-config-io-keys");
    let ok = status(ExitCode::SUCCESS);
    let refused = status(ExitCode::from(2));
    let keys = "header_timeout = \"61s\"\nlisten_backlog = 512\naccept_pause = \"150ms\"\n\
                door_grace = \"4s\"\ndoor_look = \"20ms\"\nput_pieces_queued = 6\n\
                oci_handshake_timeout = \"12s\"\ns3_part_bytes = \"32MiB\"\n\
                s3_multipart_threshold = \"32MiB\"\ns3_chunks_waiting = 2\ns3_endpoint_attempts = 4\n\
                dir_copy_chunk = \"2MiB\"\ndir_read_chunk = \"128KiB\"";
    assert_eq!(both(&node.file(keys, "", "")), (ok.clone(), refused));
    assert_eq!(both(&node.file("", "", "")), (ok.clone(), ok));
}

/// Asserts that a storage-node file whose `[hot_store]` has `endpoints` in place of `endpoint` is
/// read by this release and refused by the release before with status 2 (it takes a list only in
/// the `[cold_store]` of a build node), so a host that may roll back keeps `endpoint` until every
/// process has this release; and that the same file with `endpoint` is loaded by both, and so is a
/// list of one entry in place of `endpoint` by this release only.
#[test]
fn a_list_of_endpoints_is_read_here_and_refused_by_the_release_before() {
    let node = Node::new("compat-config-endpoints");
    let ok = status(ExitCode::SUCCESS);
    let refused = status(ExitCode::from(2));
    let file = node.file("", "", "");
    assert_eq!(both(&file), (ok.clone(), ok.clone()));
    let text = std::fs::read_to_string(&file).unwrap();
    for list in [
        "[\"http://hot.invalid\"]",
        "[\"http://a.invalid\", \"http://b.invalid\"]",
    ] {
        let listed = text.replacen(
            "endpoint = \"http://hot.invalid\"",
            &format!("endpoints = {list}"),
            1,
        );
        assert_ne!(listed, text);
        std::fs::write(&file, listed).unwrap();
        assert_eq!(both(&file), (ok.clone(), refused.clone()), "{list}");
    }
}
