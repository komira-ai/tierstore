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

/// The file of `file` with an `[oci]` table, `extra` in it, on a loopback address.
fn oci_file(dir: &TestDir, access: &str, extra: &str) -> String {
    let text = std::fs::read_to_string(file(dir, access, "")).unwrap();
    let text = format!("{text}\n[oci]\nlisten = \"127.0.0.1:0\"\n{extra}\n");
    let path = dir.path().join("tierstore-oci.toml");
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}

/// Asserts that a file with an `[oci]` table and no `require_auth`, which is every such file the
/// release before reads, starts on the release before and on this branch: the key is optional and
/// off by default.
#[test]
fn an_oci_file_without_require_auth_starts_on_the_release_before_and_on_this_branch() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-oci-config");
    let access = private(&dir, "access.key", "AKID\nsecret\n");
    let plain = oci_file(&dir, &access, "");
    assert!(succeeded(tierstore_prev::server::cli::main(checking(
        &plain
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&plain))));
}

/// Asserts the other direction, for the one new key: a file with `oci.require_auth = true` starts
/// on this branch, and the release before refuses it at start (an unknown key) rather than serve
/// the front anonymously, so a rollback with the key set cannot open a front the file meant to lock.
#[test]
fn an_oci_file_with_require_auth_starts_on_this_branch_and_is_refused_by_the_release_before() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-oci-auth-config");
    let access = private(&dir, "access.key", "AKID\nsecret\n");
    let locked = oci_file(&dir, &access, "require_auth = true");
    assert!(succeeded(tierstore::server::cli::main(checking(&locked))));
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &locked
    ))));
}

/// Asserts a file with an `[oci.upstream]` table, a table this branch adds, starts on this branch
/// (the key files it names are read, and the pull-through is made, as `--check-config` does) and is
/// refused by the release before, which does not know the table: a new optional table that only a
/// file written for this branch carries. The same file without the table starts on both, as the
/// test before this one shows for a file with none of the new keys.
#[test]
fn a_file_with_an_oci_upstream_starts_on_this_branch_and_not_on_the_release_before() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-compat-upstream-config");
    let access = private(&dir, "access.key", "AKID\nsecret\n");
    let base = std::fs::read_to_string(file(&dir, &access, "")).unwrap();
    let front = "\n[oci]\nlisten = \"127.0.0.1:0\"\n";
    let upstream = "\n[oci.upstream]\nurl = \"http://127.0.0.1:5000\"\n";
    let write = |name: &str, text: String| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    let without = write("without.toml", format!("{base}{front}"));
    let with = write("with.toml", format!("{base}{front}{upstream}"));
    assert!(succeeded(tierstore_prev::server::cli::main(checking(
        &without
    ))));
    assert!(succeeded(tierstore::server::cli::main(checking(&without))));
    assert!(succeeded(tierstore::server::cli::main(checking(&with))));
    assert!(!succeeded(tierstore_prev::server::cli::main(checking(
        &with
    ))));
}
