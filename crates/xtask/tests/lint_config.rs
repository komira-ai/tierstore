//! The lints that keep production code from panicking on `unwrap` and `expect`, from reading
//! the machine's clock and from writing to the standard streams stay switched on (AGENTS.md R1,
//! R3, T10). The lints are run by `cargo clippy` in CI's `lint` job; what this test holds is
//! the configuration that job reads, so that the gate cannot be loosened without a test
//! failing.
//!
//! Catches:
//! - a lint of `[workspace.lints.clippy]` removed or lowered from `deny`;
//! - a method (`Instant::now`, `SystemTime::now`) or a macro (`println!`, `print!`,
//!   `eprintln!`, `eprint!`, `dbg!`) dropped from the banned lists of `clippy.toml`;
//! - `allow-unwrap-in-tests` or `allow-expect-in-tests` dropped, which would turn every test
//!   assertion into a finding;
//! - a check that cannot see a defect: the same check, run on a configuration with each of
//!   those removed, names each one, and finds nothing in a complete one.

use std::path::PathBuf;

/// The lints that must be `deny` in `[workspace.lints.clippy]`.
const DENIED: &[&str] = &[
    "disallowed_macros",
    "disallowed_methods",
    "disallowed_types",
    "expect_used",
    "unwrap_used",
];

/// The paths `clippy.toml` bans as methods.
const METHODS: &[&str] = &["std::time::Instant::now", "std::time::SystemTime::now"];

/// The paths `clippy.toml` bans as macros.
const MACROS: &[&str] = &[
    "std::dbg",
    "std::eprint",
    "std::eprintln",
    "std::print",
    "std::println",
];

fn read(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The lines of the table `header` (for example `[workspace.lints.clippy]`) of `text`.
fn table<'a>(text: &'a str, header: &str) -> Vec<&'a str> {
    text.lines()
        .skip_while(|line| line.trim() != header)
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .collect()
}

/// The `path` of each entry of the array `key = [ ... ]` of `text`.
fn paths<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    let open = format!("{key} = [");
    text.lines()
        .skip_while(|line| !line.starts_with(&open))
        .skip(1)
        .take_while(|line| !line.starts_with(']'))
        .filter_map(|line| line.split("path = \"").nth(1))
        .filter_map(|rest| rest.split('"').next())
        .collect()
}

/// What `cargo_toml` and `clippy_toml` lack of what this test requires, one line each.
fn missing(cargo_toml: &str, clippy_toml: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lints = table(cargo_toml, "[workspace.lints.clippy]");
    for lint in DENIED {
        let wanted = format!("{lint} = \"deny\"");
        if !lints.iter().any(|line| line.trim() == wanted) {
            found.push(format!("[workspace.lints.clippy] lacks `{wanted}`"));
        }
    }
    for (key, wanted) in [
        ("disallowed-methods", METHODS),
        ("disallowed-macros", MACROS),
    ] {
        let banned = paths(clippy_toml, key);
        for path in wanted {
            if !banned.contains(path) {
                found.push(format!("clippy.toml {key} lacks {path}"));
            }
        }
    }
    for key in ["allow-unwrap-in-tests", "allow-expect-in-tests"] {
        let wanted = format!("{key} = true");
        if !clippy_toml.lines().any(|line| line.trim() == wanted) {
            found.push(format!("clippy.toml lacks `{wanted}`"));
        }
    }
    found
}

#[test]
fn the_repository_denies_unwrap_expect_real_clocks_and_standard_stream_macros() {
    let found = missing(&read("Cargo.toml"), &read("clippy.toml"));
    assert!(found.is_empty(), "{}", found.join("\n"));
}

const CARGO: &str = "[workspace.lints.clippy]\n\
    disallowed_macros = \"deny\"\n\
    disallowed_methods = \"deny\"\n\
    disallowed_types = \"deny\"\n\
    expect_used = \"deny\"\n\
    unwrap_used = \"deny\"\n\
    \n\
    [profile.dev]\n";

const CLIPPY: &str = "disallowed-methods = [\n\
    { path = \"std::time::Instant::now\", reason = \"r\" },\n\
    { path = \"std::time::SystemTime::now\", reason = \"r\" },\n\
    ]\n\
    disallowed-macros = [\n\
    { path = \"std::dbg\", reason = \"r\" },\n\
    { path = \"std::eprint\", reason = \"r\" },\n\
    { path = \"std::eprintln\", reason = \"r\" },\n\
    { path = \"std::print\", reason = \"r\" },\n\
    { path = \"std::println\", reason = \"r\" },\n\
    ]\n\
    allow-unwrap-in-tests = true\n\
    allow-expect-in-tests = true\n";

/// Asserts that a complete configuration is accepted, and that each requirement is named
/// when it is removed: a lint lowered to `warn`, a method and a macro dropped from their
/// lists, and the two test allowances dropped.
#[test]
fn a_configuration_missing_a_lint_a_ban_or_a_test_allowance_is_named() {
    assert_eq!(missing(CARGO, CLIPPY), Vec::<String>::new());

    let lowered = CARGO.replace("unwrap_used = \"deny\"", "unwrap_used = \"warn\"");
    assert_eq!(
        missing(&lowered, CLIPPY),
        vec!["[workspace.lints.clippy] lacks `unwrap_used = \"deny\"`".to_owned()]
    );

    let fewer = CLIPPY
        .replace(
            "{ path = \"std::time::SystemTime::now\", reason = \"r\" },\n",
            "",
        )
        .replace("{ path = \"std::eprintln\", reason = \"r\" },\n", "")
        .replace("allow-unwrap-in-tests = true\n", "")
        .replace("allow-expect-in-tests = true\n", "");
    assert_eq!(
        missing(CARGO, &fewer),
        vec![
            "clippy.toml disallowed-methods lacks std::time::SystemTime::now".to_owned(),
            "clippy.toml disallowed-macros lacks std::eprintln".to_owned(),
            "clippy.toml lacks `allow-unwrap-in-tests = true`".to_owned(),
            "clippy.toml lacks `allow-expect-in-tests = true`".to_owned(),
        ]
    );
}
