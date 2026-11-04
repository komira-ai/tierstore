//! A configuration file written for the release before, started on this branch: the release before
//! is the real `tierstore` library of its commit (`tierstore_prev`), and each side is asked what
//! the binary's `--check-config` asks (the settings that a start makes from the file, with the
//! key files it names read, and nothing opened). It prints nothing and exits 0 if the file starts.

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

/// A grant key as the release before wants it: an id and a secret of at least 32 bytes.
const GRANT_KEY: &str = "AKIDGRANT\n0123456789abcdef0123456789abcdef0123456789abcdef\n";

/// A key as both releases take it: the secret has 16 bytes, the least this branch asks by default.
const LONG_ENOUGH: &str = "AKID\n0123456789abcdef\n";

fn private(dir: &TestDir, name: &str, text: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    path.display().to_string()
}

/// The file of a storage node with two tiers in memory, with `extra` in its `[server]` table.
fn file(dir: &TestDir, access: &str, extra: &str) -> String {
    let text = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nproxy_id = \"p1\"\nbucket = \"tier-bucket\"\n\
         access_key_file = \"{access}\"\n{extra}\n\
         [hot_store]\nkind = \"memory\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n"
    );
    let path = dir.path().join("tierstore.toml");
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

fn checking(config: &str) -> Vec<OsString> {
    ["tierstore", "--config", config, "--check-config"]
        .map(OsString::from)
        .to_vec()
}

fn succeeded(code: ExitCode) -> bool {
    format!("{code:?}") == format!("{:?}", ExitCode::SUCCESS)
}

/// Asserts that a file with a grant key file and a grant lifetime, which the release before starts
/// (so it is a file of that release), starts on this branch too: the two keys of the removed read
/// grant are still accepted, and ignored, so that an old unit starts. The grant key file is also
/// missing in a second file, which this branch starts because it does not read it, and the release
/// before refuses because it does.
#[test]
fn a_file_written_for_the_release_before_with_read_grant_keys_starts_on_this_branch() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-grant-config");
    let access = private(&dir, "access.key", LONG_ENOUGH);
    let grant = private(&dir, "grant.key", GRANT_KEY);
    let extra = format!("grant_key_files = [\"{grant}\"]\ngrant_max_lifetime = \"5m\"\n");
    let old = file(&dir, &access, &extra);
    assert!(
        succeeded(tierstore_prev::server::cli::main(checking(&old))),
        "the release before starts its own file"
    );
    assert!(
        succeeded(tierstore::server::cli::main(checking(&old))),
        "this branch starts the file of the release before"
    );

    let gone = dir.path().join("no-such-grant.key").display().to_string();
    let extra = format!("grant_key_files = [\"{gone}\"]\n");
    let missing = file(&dir, &access, &extra);
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &missing
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&missing))));
}

/// Asserts the other direction: a file with no key of the read grant, which is what this branch
/// documents, starts on the release before and on this branch.
#[test]
fn a_file_without_read_grant_keys_starts_on_the_release_before_and_on_this_branch() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-plain-config");
    let access = private(&dir, "access.key", LONG_ENOUGH);
    let plain = file(&dir, &access, "");
    assert!(succeeded(tierstore_prev::server::cli::main(checking(
        &plain
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&plain))));
}

/// Asserts that a file with the `listen` key of `[server]` and the `[oci]` table, the shape the
/// release before reads, starts on this branch as it does on that release: this branch reads the
/// two as an `s3` and an `oci` interface. A file with an `[oci]` table that has no `listen` is
/// refused by both.
#[test]
fn a_file_with_the_oci_table_of_the_release_before_starts_on_both() {
    use std::io::Write;

    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-oci-config");
    let access = private(&dir, "access.key", LONG_ENOUGH);
    let with = file(&dir, &access, "");
    let mut text = std::fs::OpenOptions::new()
        .append(true)
        .open(&with)
        .unwrap();
    text.write_all(b"\n[oci]\nlisten = \"127.0.0.1:0\"\nprefix = \"images/\"\n")
        .unwrap();
    assert!(succeeded(tierstore_prev::server::cli::main(checking(
        &with
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&with))));

    let bare = file(&dir, &access, "");
    let mut text = std::fs::OpenOptions::new()
        .append(true)
        .open(&bare)
        .unwrap();
    text.write_all(b"\n[oci]\nprefix = \"images/\"\n").unwrap();
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &bare
    ))));
    assert!(!succeeded(tierstore::server::cli::main(checking(&bare))));
}

/// Asserts the change of the default minimum on a file of the release before: a file whose key has
/// a secret of 6 bytes, which the release before starts, is refused on this branch with the
/// minimum at its default, which is the one deliberate break; with `min_secret_bytes = 0` in the
/// file it starts on this branch again. A file this branch prints at the default minimum has no
/// `min_secret_bytes` key, and starts on both. A file with `min_secret_bytes = 6` is only for this
/// branch (the release before does not know the key), and starts on it.
#[test]
fn a_short_secret_of_the_release_before_is_refused_here_until_the_minimum_is_lowered() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-short-secret");
    let short = private(&dir, "short.key", "AKID\nsecret\n");
    let old = file(&dir, &short, "");
    assert!(succeeded(tierstore_prev::server::cli::main(checking(&old))));
    assert!(!succeeded(tierstore::server::cli::main(checking(&old))));

    let lowered = file(&dir, &short, "min_secret_bytes = 0\n");
    assert!(succeeded(tierstore::server::cli::main(checking(&lowered))));
    let six = file(&dir, &short, "min_secret_bytes = 6\n");
    assert!(succeeded(tierstore::server::cli::main(checking(&six))));
}

/// A self-signed certificate that names `tierstore.test`, and its key, written under `dir`; the
/// paths of the two files. The key is private to its owner, as the front requires.
fn certificate(dir: &TestDir) -> (String, String) {
    let made = rcgen::generate_simple_self_signed(vec!["tierstore.test".to_owned()]).unwrap();
    let cert = private(dir, "front.pem", &made.cert.pem());
    let key = private(dir, "front.key", &made.signing_key.serialize_pem());
    (cert, key)
}

/// Asserts that a file with `tls_cert_file` and `tls_key_file`, the keys of the S3 front's TLS, is
/// started by this branch and is refused by the release before, which does not know the keys (its
/// tables refuse an unknown key, as for every key added since): an operator gives the keys after
/// the upgrade. The same file without the two keys starts on both.
#[test]
fn a_file_with_the_s3_tls_keys_starts_on_this_branch_and_a_file_without_them_on_both() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-tls-config");
    let access = private(&dir, "access.key", "AKID\na-long-enough-secret\n");
    let (cert, key) = certificate(&dir);

    let without = file(&dir, &access, "");
    assert!(succeeded(tierstore_prev::server::cli::main(checking(
        &without
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&without))));

    let extra = format!("tls_cert_file = \"{cert}\"\ntls_key_file = \"{key}\"\n");
    let with = file(&dir, &access, &extra);
    assert!(
        succeeded(tierstore::server::cli::main(checking(&with))),
        "this branch starts a file with the TLS keys"
    );
    assert!(
        !succeeded(tierstore_prev::server::cli::main(checking(&with))),
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
    use std::io::Write;

    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-oci-auth-config");
    let access = private(&dir, "access.key", LONG_ENOUGH);
    let locked = file(&dir, &access, "");
    let mut text = std::fs::OpenOptions::new()
        .append(true)
        .open(&locked)
        .unwrap();
    text.write_all(
        b"\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"], require_auth = true }\n",
    )
    .unwrap();
    assert!(succeeded(tierstore::server::cli::main(checking(&locked))));
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &locked
    ))));

    let open = file(&dir, &access, "");
    let mut text = std::fs::OpenOptions::new()
        .append(true)
        .open(&open)
        .unwrap();
    text.write_all(
        b"\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"] }\n",
    )
    .unwrap();
    assert!(succeeded(tierstore::server::cli::main(checking(&open))));
}

/// Asserts the one key `stage_read` of an `oci` interface: a file with it set starts on this
/// branch, and the release before refuses the file at start (an unknown param, and it does not
/// know `[[interface]]` either); the same file with the key left out starts on this branch with
/// the default size of a read.
#[test]
fn an_oci_interface_with_stage_read_starts_on_this_branch_and_is_refused_by_the_release_before() {
    use std::io::Write;

    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-oci-stage-config");
    let access = private(&dir, "access.key", LONG_ENOUGH);
    let sized = file(&dir, &access, "");
    let mut text = std::fs::OpenOptions::new()
        .append(true)
        .open(&sized)
        .unwrap();
    text.write_all(
        b"\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"], stage_read = 4096 }\n",
    )
    .unwrap();
    assert!(succeeded(tierstore::server::cli::main(checking(&sized))));
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &sized
    ))));

    let plain = file(&dir, &access, "");
    let mut text = std::fs::OpenOptions::new()
        .append(true)
        .open(&plain)
        .unwrap();
    text.write_all(
        b"\n[[interface]]\ntype = \"oci\"\nparams = { addresses = [\"127.0.0.1:0\"] }\n",
    )
    .unwrap();
    assert!(succeeded(tierstore::server::cli::main(checking(&plain))));
}

/// Asserts the one key `upstream` of an `oci` interface: a file with it starts on this branch (the
/// pull-through is made, as `--check-config` does) and the release before refuses it, as it does
/// not know `[[interface]]` at all, rather than serve a front that never pulls. The same file
/// without the key starts on this branch, which serves the front as before.
#[test]
fn an_oci_interface_with_an_upstream_starts_on_this_branch_and_is_refused_by_the_release_before() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-upstream-config");
    let access = private(&dir, "access.key", LONG_ENOUGH);
    let base = std::fs::read_to_string(file(&dir, &access, "")).unwrap();
    let write = |name: &str, front: &str| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            format!("{base}\n[[interface]]\ntype = \"oci\"\nparams = {front}\n"),
        )
        .unwrap();
        path.display().to_string()
    };
    let without = write("without.toml", "{ addresses = [\"127.0.0.1:0\"] }");
    let with = write(
        "with.toml",
        "{ addresses = [\"127.0.0.1:0\"], upstream = { url = \"http://127.0.0.1:5000\" } }",
    );
    assert!(succeeded(tierstore::server::cli::main(checking(&without))));
    assert!(succeeded(tierstore::server::cli::main(checking(&with))));
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &with
    ))));
}
