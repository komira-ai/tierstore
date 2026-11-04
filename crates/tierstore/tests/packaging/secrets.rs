//! What of a unit's command line and its file could hold a secret: names, not values.

/// The words in the name of a flag or a key that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// The names of the keys of `table`, in its tables and arrays of tables too, that say they hold
/// a secret.
fn secret_keys(table: &toml::Table, found: &mut Vec<String>) {
    for (key, value) in table {
        if SECRET_WORDS.iter().any(|name| key.contains(*name)) {
            found.push(key.clone());
        }
        match value {
            toml::Value::Table(inner) => secret_keys(inner, found),
            toml::Value::Array(items) => {
                for inner in items.iter().filter_map(toml::Value::as_table) {
                    secret_keys(inner, found);
                }
            }
            _ => {}
        }
    }
}

/// Asserts that no flag of `words` speaks of a secret, and that the keys of the file `name`
/// that do are the four that name a file (`_file` or `_files`): the full key, the read-only
/// keys and the credentials of the two tiers.
pub fn assert_no_secret_in_the_flags_or_the_file(words: &[String], file: &str) {
    for flag in words.iter().filter(|word| word.starts_with("--")) {
        assert!(
            !SECRET_WORDS.iter().any(|secret| flag.contains(secret)),
            "{flag}"
        );
    }
    let file: toml::Table = file.parse().expect("TOML");
    let mut keys = Vec::new();
    secret_keys(&file, &mut keys);
    assert_eq!(keys.len(), 4, "{keys:?}");
    for key in keys {
        assert!(
            key.ends_with("_file") || key.ends_with("_files"),
            "{key} is not a file"
        );
    }
}
