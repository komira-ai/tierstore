//! A configuration file written for the release before, started on this branch: the release before
//! is the real `tierstore` library of its commit (`tierstore_prev`), and each side is asked what
//! the binary's `--check-config` asks (the settings that a start makes from the file, with the
//! key files it names read, and nothing opened). It prints nothing and exits 0 if the file starts.

use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

/// A grant key as the release before wants it: an id and a secret of at least 32 bytes.
const GRANT_KEY: &str = "AKIDGRANT\n0123456789abcdef0123456789abcdef0123456789abcdef\n";

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
    let access = private(&dir, "access.key", "AKID\nsecret\n");
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
/// writes (`--print-config` leaves them out), starts on the release before and on this branch.
#[test]
fn a_file_without_read_grant_keys_starts_on_the_release_before_and_on_this_branch() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-plain-config");
    let access = private(&dir, "access.key", "AKID\nsecret\n");
    let plain = file(&dir, &access, "");
    assert!(succeeded(tierstore_prev::server::cli::main(checking(
        &plain
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&plain))));
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
/// the upgrade. The same file without the two keys starts on both, and is what `--print-config`
/// writes for a node that does not use TLS.
#[test]
fn a_file_with_the_s3_tls_keys_starts_on_this_branch_and_a_file_without_them_on_both() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-tls-config");
    let access = private(&dir, "access.key", "AKID\nsecret\n");
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
