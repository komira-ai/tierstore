//! The params file.

use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;

use super::{Params, ParamsError};

const GOOD: &str = "promotion_bound_secs = 900\n\
\"updater.staged_release\" = \"/var/lib/updater/releases/tierstore/abc\"\n\
\"updater.target_version\" = \"0.4.0\"\n";

fn with(extra: &str) -> String {
    format!("{GOOD}{extra}")
}

fn refused(text: &str) -> String {
    Params::parse(text).unwrap_err().to_string()
}

/// Asserts that the required keys are read, the optional ones take their defaults, and a reserved
/// key the hook does not know, or a scalar key it does not read, is ignored. It would catch a hook
/// that refuses a params file because the updater added a key.
#[test]
fn the_required_keys_are_read_and_the_rest_default() {
    let params = Params::parse(&with("\"updater.something_new\" = true\nother = 1.5\n")).unwrap();
    assert_eq!(params.promotion_bound_secs, 900);
    assert_eq!(params.defer_retry_secs, 300);
    assert_eq!(params.conntrack_percent, 80);
    assert_eq!(params.promotion_ceiling_secs, None);
    assert_eq!(
        params.staged_release,
        PathBuf::from("/var/lib/updater/releases/tierstore/abc")
    );
    assert_eq!(params.target_version, "0.4.0");
    assert!(!params.declared_restart);
}

/// Asserts that the optional keys are read when given. It would catch a default that wins over
/// the declaration, and a ceiling that is dropped before the verify step could be tested with it.
#[test]
fn the_optional_keys_are_read_when_given() {
    let params = Params::parse(&with(
        "defer_retry_secs = 0\nconntrack_percent = 50\n\"updater.promotion_ceiling_secs\" = 3600\n",
    ))
    .unwrap();
    assert_eq!(params.defer_retry_secs, 0);
    assert_eq!(params.conntrack_percent, 50);
    assert_eq!(params.promotion_ceiling_secs, Some(3600));
}

/// Asserts each refusal and its message. It would catch a params file the hook acts on that the
/// updater could not have written: a path with `..`, a negative wait, a bound of zero.
#[test]
fn a_wrong_params_file_is_refused() {
    assert!(refused("promotion_bound_secs = ").contains("not a TOML table"));
    assert!(refused(&with("list = [1]\n")).contains("not a scalar"));
    let no_bound = GOOD.replace("promotion_bound_secs = 900\n", "");
    assert!(refused(&no_bound).contains("\"promotion_bound_secs\" is required"));
    assert!(refused(&GOOD.replace("900", "-5")).contains("whole number"));
    assert!(refused(&GOOD.replace("900", "\"x\"")).contains("whole number"));
    assert!(refused(&GOOD.replace("900", "0")).contains("from 1 to 86400"));
    assert!(refused(&GOOD.replace("900", "86401")).contains("from 1 to 86400"));
    assert!(refused(&with("conntrack_percent = 101\n")).contains("from 1 to 100"));
    assert!(refused(&with("defer_retry_secs = 86401\n")).contains("from 0 to 86400"));
    let relative = GOOD.replace("/var/lib/updater/releases/tierstore/abc", "releases/abc");
    assert!(refused(&relative).contains("absolute path of plain components"));
    let dotted = GOOD.replace("tierstore/abc", "tierstore/../abc");
    assert!(refused(&dotted).contains("plain components"));
    assert!(refused(&GOOD.replace("0.4.0", "-1")).contains("not a version"));
    assert!(refused(&GOOD.replace("0.4.0", "a..b")).contains("not a version"));
    assert!(refused(&GOOD.replace("0.4.0", "a b")).contains("not a version"));
    assert!(refused(&GOOD.replace("0.4.0", "")).contains("not a version"));
    assert!(refused(&GOOD.replace("\"0.4.0\"", "4")).contains("must be a string"));
    let no_staged = GOOD.replace(
        "\"updater.staged_release\" = \"/var/lib/updater/releases/tierstore/abc\"\n",
        "",
    );
    assert!(refused(&no_staged).contains("updater.staged_release"));
    let no_version = GOOD.replace("\"updater.target_version\" = \"0.4.0\"\n", "");
    assert!(refused(&no_version).contains("updater.target_version"));
}

/// Asserts that a file is read from disk and a missing one is a read error. It would catch a
/// reader that treats a missing params file as an empty one.
#[test]
fn a_params_file_is_read_from_disk() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-hook-params");
    let path = dir.path().join("params.toml");
    std::fs::write(&path, GOOD).unwrap();
    assert_eq!(Params::read(&path).unwrap().target_version, "0.4.0");
    let missing = Params::read(&dir.path().join("none.toml")).unwrap_err();
    assert!(matches!(missing, ParamsError::Read(_)));
}

/// Asserts that `mode = "restart"` is read as a forced restart, and that the key is the one string
/// a declaration may set: another word, another case, a number, a boolean and an array are each
/// refused with a message that names the key. It would catch a hook that hands over on a host
/// whose packaging asked for a restart because a typo was read as "no preference".
#[test]
fn mode_restart_is_the_one_value_of_the_mode_key() {
    let params = Params::parse(&with("mode = \"restart\"\n")).unwrap();
    assert!(params.declared_restart);
    for bad in [
        "mode = \"handover\"\n",
        "mode = \"Restart\"\n",
        "mode = \"\"\n",
        "mode = 1\n",
        "mode = true\n",
    ] {
        let why = refused(&with(bad));
        assert!(why.contains("params key \"mode\""), "{bad}: {why}");
        assert!(
            why.contains("must be the string \"restart\""),
            "{bad}: {why}"
        );
    }
    let why = refused(&with("mode = [\"restart\"]\n"));
    assert!(why.contains("\"mode\" is not a scalar"), "{why}");
}
