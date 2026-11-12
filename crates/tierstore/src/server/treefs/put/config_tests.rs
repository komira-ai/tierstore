//! The `[fs.put]` table: not served unless enabled, its defaults, every key taken, and each refusal
//! by the key it names.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::Duration;

use tracing::Level;
use tracing::subscriber::with_default;

#[cfg(target_os = "linux")]
use super::super::store::PutSettings;
use super::super::testing::Warnings;
use super::PutConfig;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::treefs::config::from_text;

/// A file with the needed keys of `[fs]` and the table `[fs.put]` of `put`.
fn file(put: &str) -> String {
    format!(
        "[fs]\nclient_group = \"7\"\nowner = \"1000:1001\"\n\
         fill_socket = \"/run/tierstore/fill.sock\"\ncache_path = \"/var/cache/tierstore\"\n\
         [fs.put]\n{put}"
    )
}

/// The put directory of the file with the table `put`.
fn put_of(put: &str) -> Option<PutConfig> {
    from_text(&file(put), "").unwrap().put
}

/// The words of the refusal of the file with the table `put`.
fn refused(put: &str) -> String {
    from_text(&file(put), "").unwrap_err().to_string()
}

/// What an enabled table needs.
const ENABLED: &str =
    "enabled = true\nprefixes = [\"cas/b/\"]\nstage_dir = \"/var/cache/tierstore/stage\"\n";

/// Asserts that a file with no `[fs.put]`, an empty one, or one that is not enabled has no put
/// directory, whatever else the table holds.
#[test]
fn the_put_directory_is_not_served_unless_enabled() {
    let none =
        "[fs]\nclient_group = \"7\"\nowner = \"1:2\"\nfill_socket = \"/f\"\ncache_path = \"/c\"\n";
    assert_eq!(from_text(none, "").unwrap().put, None);
    assert_eq!(put_of(""), None);
    assert_eq!(put_of("enabled = false\nprefixes = [\"cas/b/\"]\n"), None);
}

/// Asserts that an enabled table with only its needed keys gets the defaults: a deadline of 30 s,
/// a stage TTL of 12 hours, a committed TTL of 10 minutes, and no limit.
#[cfg(target_os = "linux")]
#[test]
fn an_enabled_table_with_its_needed_keys_gets_the_defaults() {
    assert_eq!(
        put_of(ENABLED),
        Some(PutConfig {
            stage_dir: PathBuf::from("/var/cache/tierstore/stage"),
            store: PutSettings {
                prefixes: vec!["cas/b/".to_owned()],
                max_object_bytes: None,
                max_dir_bytes: None,
                max_dir_files: None,
                min_free_bytes: None,
                stage_ttl: Duration::from_secs(43_200),
                committed_ttl: Duration::from_secs(600),
            },
            commit_deadline: Duration::from_secs(30),
        })
    );
}

/// Asserts that every key set is taken, each at an end of its range.
#[cfg(target_os = "linux")]
#[test]
fn every_key_is_taken_when_it_is_set() {
    let text = format!(
        "{ENABLED}max_object_bytes = 1\nmax_dir_bytes = 2\nmax_dir_files = 3\nmin_free_bytes = 0\n\
         commit_deadline_secs = 3600\nstage_ttl_secs = 901\ncommitted_ttl_secs = 604800\n"
    );
    let put = put_of(&text).unwrap();
    assert_eq!(
        put.store,
        PutSettings {
            prefixes: vec!["cas/b/".to_owned()],
            max_object_bytes: Some(1),
            max_dir_bytes: Some(2),
            max_dir_files: Some(3),
            min_free_bytes: Some(0),
            stage_ttl: Duration::from_secs(901),
            committed_ttl: Duration::from_secs(604_800),
        }
    );
    assert_eq!(put.commit_deadline, Duration::from_secs(3600));
    let low = put_of(&format!(
        "{ENABLED}commit_deadline_secs = 1\ncommitted_ttl_secs = 1\nstage_ttl_secs = 604800\n"
    ))
    .unwrap();
    assert_eq!(
        (
            low.commit_deadline,
            low.store.committed_ttl,
            low.store.stage_ttl
        ),
        (
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(604_800)
        )
    );
}

/// Asserts each refusal of an enabled table, by the key it names: the prefixes and the stage
/// directory left out, no prefix, a prefix that is not one (no `/` at its end, an empty
/// component, white space in it) or one the proxy keeps for itself, a stage directory that is not
/// absolute.
#[test]
fn each_refusal_of_an_enabled_table_names_its_key() {
    let stage = "stage_dir = \"/s\"\n";
    for (put, said) in [
        (
            format!("enabled = true\n{stage}"),
            "fs.put.prefixes: it is needed",
        ),
        (
            "enabled = true\nprefixes = [\"cas/b/\"]\n".to_owned(),
            "fs.put.stage_dir: it is needed",
        ),
        (
            format!("enabled = true\nprefixes = []\n{stage}"),
            "fs.put.prefixes: is empty: no key could be written",
        ),
        (
            format!("enabled = true\nprefixes = [\"cas/b\"]\n{stage}"),
            "fs.put.prefixes: \"cas/b\" is not a key prefix of names joined by `/` and ending in `/`",
        ),
        (
            format!("enabled = true\nprefixes = [\"cas//\"]\n{stage}"),
            "fs.put.prefixes: \"cas//\" is not a key prefix of names joined by `/` and ending in `/`",
        ),
        (
            format!("enabled = true\nprefixes = [\"cas b/\"]\n{stage}"),
            "fs.put.prefixes: \"cas b/\" is not a key prefix of names joined by `/` and ending in `/`",
        ),
        (
            format!("enabled = true\nprefixes = [\"cas/b/\", \"_tiering/x/\"]\n{stage}"),
            "fs.put.prefixes: \"_tiering/x/\" is under a prefix the proxy keeps for itself",
        ),
        (
            "enabled = true\nprefixes = [\"cas/b/\"]\nstage_dir = \"stage\"\n".to_owned(),
            "fs.put.stage_dir: must be an absolute path",
        ),
    ] {
        assert!(refused(&put).contains(said), "{put}: {}", refused(&put));
    }
}

/// Asserts each refusal of a number, enabled or not, by the key it names: a limit of 0, and a
/// deadline or a TTL out of its range.
#[test]
fn each_number_out_of_its_range_is_refused_by_its_key() {
    for (put, said) in [
        (
            "max_object_bytes = 0\n",
            "fs.put.max_object_bytes: must be at least 1",
        ),
        (
            "max_dir_bytes = 0\n",
            "fs.put.max_dir_bytes: must be at least 1",
        ),
        (
            "max_dir_files = 0\n",
            "fs.put.max_dir_files: must be at least 1",
        ),
        ("commit_deadline_secs = 0\n", "fs.put.commit_deadline_secs"),
        (
            "commit_deadline_secs = 3601\n",
            "fs.put.commit_deadline_secs",
        ),
        ("stage_ttl_secs = 900\n", "fs.put.stage_ttl_secs"),
        ("stage_ttl_secs = 604801\n", "fs.put.stage_ttl_secs"),
        ("committed_ttl_secs = 0\n", "fs.put.committed_ttl_secs"),
        ("committed_ttl_secs = 604801\n", "fs.put.committed_ttl_secs"),
    ] {
        assert!(refused(put).contains(said), "{put}: {}", refused(put));
        let enabled = format!("{ENABLED}{put}");
        assert!(refused(&enabled).contains(said), "{enabled}");
    }
}

/// Asserts that a key of `[fs.put]` this release does not know is ignored with one warning that
/// names it, and the file loads (a release does not refuse the file of the next).
#[test]
fn an_unknown_put_key_is_ignored_and_the_file_loads() {
    let warnings = Warnings::default();
    let put = with_default(LineLog::new(warnings.clone(), Level::WARN), || {
        from_text(&file("futuristic = 3\n"), "").unwrap().put
    });
    assert_eq!(put, None);
    assert_eq!(
        warnings.text(),
        "tierstore fs: fs.put.futuristic is not a key of this release; ignored\n"
    );
}

/// Asserts that off Linux an enabled table is refused: only the FUSE frontend serves `put/` in
/// this release.
#[cfg(not(target_os = "linux"))]
#[test]
fn an_enabled_table_is_refused_off_linux() {
    assert!(refused(ENABLED).contains("fs.put.enabled"));
}
