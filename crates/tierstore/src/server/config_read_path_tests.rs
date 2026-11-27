//! `[cache]` in a build node's file: the root and the budget it gives, and each refusal, which
//! names its key.

use std::path::PathBuf;

use tierstore_journal::BootId;
use tierstore_testkit::dir::TestDir;

use super::ReadPath;
use crate::server::cache_cli_tests::QUICK;
use crate::server::config::settings_from_text;
use crate::server::config_unknown::unknown_key_warnings;
use crate::server::local_cache::large::testing::{build_node_file, cache_settings};

/// The message of the refusal of the file `text`.
fn refusal(text: &str) -> String {
    match settings_from_text(text, QUICK) {
        Ok(_) => panic!("the file loads"),
        Err(why) => why.to_string(),
    }
}

/// A build node's file under `dir` over an S3 cold store, with `cache` as its `[cache]` table.
fn file(dir: &TestDir, cache: &str) -> String {
    build_node_file(dir.path(), "http://192.0.2.1:9000", cache)
}

/// Asserts that `[cache]` gives its root and its budget, and the boot id file of Linux.
#[test]
fn a_cache_table_gives_the_root_and_the_budget() {
    let dir = TestDir::under(std::env::temp_dir(), "tsreadpath-keys");
    let settings = cache_settings(&file(&dir, "root = \"/srv/cache\"\nbudget = \"185GiB\""));
    assert_eq!(
        settings.read_path,
        Some(ReadPath {
            root: PathBuf::from("/srv/cache"),
            budget: 185 << 30,
            boot_id_file: PathBuf::from(BootId::LINUX_PATH),
        })
    );
    let one = cache_settings(&file(&dir, "root = \"/srv/cache\"\nbudget = \"1B\""));
    assert_eq!(one.read_path.unwrap().budget, 1);
}

/// Asserts that a file without `[cache]` builds no read path.
#[test]
fn a_file_without_the_table_has_no_read_path() {
    let dir = TestDir::under(std::env::temp_dir(), "tsreadpath-none");
    assert_eq!(cache_settings(&file(&dir, "")).read_path, None);
}

/// Asserts that a missing root, a relative root, a missing budget and a zero budget are each
/// refused, naming the key.
#[test]
fn a_missing_root_or_zero_budget_is_refused_naming_the_key() {
    let dir = TestDir::under(std::env::temp_dir(), "tsreadpath-refused");
    let cases = [
        ("budget = \"1GiB\"", "cache.root: it is needed"),
        (
            "root = \"srv/cache\"\nbudget = \"1GiB\"",
            "cache.root: must be an absolute path",
        ),
        ("root = \"/srv/cache\"", "cache.budget: it is needed"),
        (
            "root = \"/srv/cache\"\nbudget = \"0B\"",
            "cache.budget: 0 is not between 1 and 18446744073709551615",
        ),
    ];
    for (table, why) in cases {
        assert_eq!(refusal(&file(&dir, table)), why, "{table}");
    }
}

/// Asserts that the table is refused, naming `cache.root`, over a cold store that is not S3, and,
/// naming `cache`, in a file that is not a build node's (a storage node's).
#[test]
fn the_table_is_refused_over_a_cold_store_not_s3_and_outside_a_build_node() {
    let dir = TestDir::under(std::env::temp_dir(), "tsreadpath-shape");
    let table = "root = \"/srv/cache\"\nbudget = \"1GiB\"";
    let text = file(&dir, table);
    let start = text.find("[cold_store]").unwrap();
    let end = start + text[start..].find("\n\n").unwrap();
    let memory = format!(
        "{}[cold_store]\nkind = \"memory\"{}",
        &text[..start],
        &text[end..]
    );
    let why = refusal(&memory);
    assert!(
        why.starts_with("cache.root: the new cache reads the cold store"),
        "{why}"
    );
    assert!(
        why.ends_with("it needs [cold_store] kind = \"s3\""),
        "{why}"
    );
    let storage = memory.replacen(
        "[local_cache]",
        "[hot_store]\nkind = \"memory\"\n\n[local_cache]",
        1,
    );
    let why = refusal(&storage);
    assert!(
        why.starts_with("cache: the new cache is a build node's"),
        "{why}"
    );
}

/// Asserts that a key of `[cache]` that this release does not know is one warning naming it,
/// and that `root` and `budget` are known.
#[test]
fn an_unknown_key_of_the_table_is_one_warning() {
    let dir = TestDir::under(std::env::temp_dir(), "tsreadpath-unknown");
    let text = file(
        &dir,
        "root = \"/srv/cache\"\nbudget = \"1GiB\"\nshared_socket = \"/s\"",
    );
    let warnings = unknown_key_warnings(&toml::from_str(&text).unwrap());
    assert_eq!(
        warnings,
        ["tierstore: config key cache.shared_socket is not known to this release and is ignored"]
    );
    assert_eq!(cache_settings(&text).read_path.unwrap().budget, 1 << 30);
}
