//! The keys of the file that this release does not know: one warning each, and the file loads.

use super::{fields_of, unknown_key_warnings};
use crate::server::config::{TierTable, from_text};
use crate::server::config_policy::Policy;
use crate::server::config_testing::{DEFAULTS, GOLDEN, MINIMAL};

/// The warnings for the file `text`.
fn warnings(text: &str) -> Vec<String> {
    unknown_key_warnings(&toml::from_str(text).unwrap())
}

/// The warning for the key `key`.
fn warning(key: &str) -> String {
    format!("tierstore: config key {key} is not known to this release and is ignored")
}

/// Asserts that a key of the format the release knows, given with a value, is no warning: the
/// golden file has every key of the storage node, and the minimal one has the least.
#[test]
fn a_file_of_known_keys_has_no_warning() {
    assert_eq!(warnings(GOLDEN), Vec::<String>::new());
    assert_eq!(warnings(MINIMAL), Vec::<String>::new());
}

/// Asserts that an unknown key of each table is named by its table and its name, in one warning
/// with exactly this text, and that the file with it makes the settings the file without it does.
#[test]
fn an_unknown_key_is_one_warning_naming_it_and_the_node_starts() {
    let cases = [
        ("[server]", "server.new_knob"),
        ("[hot_store]", "hot_store.new_knob"),
        ("[cold_store]", "cold_store.new_knob"),
        ("[oci]", "oci.new_knob"),
    ];
    let plain = from_text(GOLDEN, DEFAULTS).unwrap();
    for (table, key) in cases {
        let text = GOLDEN.replacen(table, &format!("{table}\nnew_knob = \"x\""), 1);
        assert_eq!(warnings(&text), [warning(key)], "{table}");
        assert_eq!(from_text(&text, DEFAULTS).unwrap(), plain, "{table}");
    }
}

/// Asserts the top level and the nested forms: a key outside every table, a table the release does
/// not have (one warning, whatever is in it, and the value is not shown), a table inside a known
/// table, `[local_cache]` and `[[policy]]` keys, and a key given by two policies named once.
#[test]
fn unknown_tables_and_nested_keys_are_named_once_each() {
    let text = format!(
        "future_flag = true\n{MINIMAL}\n[future]\nsecret_looking_value = 1\n[future.inner]\nx = 1\n\
         [server.extra]\ndeep = \"hidden-value\"\n\
         [[policy]]\nprefix = \"a/\"\nnew_rule = 1\n[[policy]]\nprefix = \"b/\"\nnew_rule = 2\n"
    );
    assert_eq!(
        warnings(&text),
        [
            warning("future"),
            warning("future_flag"),
            warning("policy.new_rule"),
            warning("server.extra"),
        ]
    );
    let cache = "[local_cache]\npath = \"/c\"\nnew_cache_key = 1\n";
    assert_eq!(
        warnings(&format!("{MINIMAL}\n{cache}")),
        [warning("local_cache.new_cache_key")]
    );
    assert!(!warnings(&text).concat().contains("hidden-value"));
}

/// Asserts that a key this release has, with a value of the wrong type, is still refused (and
/// not warned about), naming the key, alongside an unknown key that is only warned about.
#[test]
fn a_mistyped_known_key_is_refused_though_an_unknown_one_is_not() {
    let text = MINIMAL.replacen("[server]", "[server]\nnew_knob = 1\nslot = \"x\"", 1);
    let why = from_text(&text, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("slot"), "{why}");
    assert!(!why.contains("new_knob"), "{why}");
    assert_eq!(warnings(&text), [warning("server.new_knob")]);
}

/// Asserts that a key that is not known and whose name says it holds a secret is still refused,
/// as it was before unknown keys were let stand.
#[test]
fn an_unknown_key_that_looks_like_a_secret_is_still_refused() {
    let text = MINIMAL.replacen("[server]", "[server]\nsecret_access_key = \"x\"", 1);
    let why = from_text(&text, DEFAULTS).unwrap_err().to_string();
    assert_eq!(
        why,
        "server.secret_access_key: a secret is never in the file; give the path of the file that \
         holds it (a key whose name ends in _file)"
    );
}

/// Asserts that the names `fields_of` finds are those of the struct: the keys a table reads,
/// under the names the file gives them.
#[test]
fn the_keys_of_a_table_are_the_fields_of_its_struct() {
    assert_eq!(fields_of::<Policy>(), ["prefix", "cache"]);
    assert!(fields_of::<TierTable>().contains(&"journal_retention"));
    assert!(fields_of::<TierTable>().contains(&"endpoints"));
    // A type that is not a struct has no fields.
    assert!(fields_of::<u8>().is_empty());
}

/// Asserts that a warning names the key and never shows its value: a key of a name no rule knows
/// for a secret, given a value, is only named.
#[test]
fn a_warning_never_shows_the_value_of_the_key() {
    let text = MINIMAL.replacen("[server]", "[server]\npasskey = \"SENTINEL\"", 1);
    assert_eq!(warnings(&text), [warning("server.passkey")]);
}
