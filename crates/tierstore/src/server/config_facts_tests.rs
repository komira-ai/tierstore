//! `server.proxy_id_from_facts`: the id a start and `--check-config` make from the facts file,
//! what each is refused with, and that the loaders of text read no file.

use std::ffi::OsString;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{facts_path, proxy_id_of};
use crate::server::config::{File, ServerTable, from_text, settings_from_text};
use crate::server::config_testing::{DEFAULTS, MINIMAL};
use crate::server::config_unknown::{fields_of, unknown_key_warnings};
use crate::server::launch::{Refused, resolve};

/// The storage-node file `MINIMAL` with `proxy_id = "p1"` replaced by `line`.
fn with_id(line: &str) -> String {
    MINIMAL.replacen("proxy_id        = \"p1\"", line, 1)
}

/// A scratch directory with the file `text` in it, with the facts file `facts` (when given),
/// and the arguments of a start of it with `extra` after.
fn rig(name: &str, facts: Option<&str>, extra: &[&str]) -> (TestDir, Vec<OsString>) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let facts_file = dir.path().join("facts.json");
    if let Some(facts) = facts {
        std::fs::write(&facts_file, facts).unwrap();
    }
    let line = format!(
        "proxy_id_from_facts = {{ path = {:?}, prefix = \"tierstore-\" }}",
        facts_file.display().to_string()
    );
    let file = dir.path().join("tierstore.toml");
    std::fs::write(&file, with_id(&line)).unwrap();
    let mut args: Vec<OsString> = vec!["tierstore".into(), "--config".into(), file.into()];
    args.extend(extra.iter().map(OsString::from));
    (dir, args)
}

/// What a start of `args` is refused with.
fn refusal(args: Vec<OsString>) -> String {
    match resolve(args).unwrap_err() {
        Refused::Config(why) => why.to_string(),
        Refused::Usage(usage) => panic!("{usage}"),
    }
}

/// Asserts that a start and `--check-config` of a file with `proxy_id_from_facts` both read the
/// facts file, that the id is the prefix and the host id, that a field the release does not know
/// of the facts file is ignored, and that the file is read again at the next start.
#[test]
fn a_start_and_a_check_make_the_proxy_id_from_the_prefix_and_the_host_id_of_the_facts_file() {
    let facts = r#"{"schema":1,"host_id":"web-04","host_group":"web","platform":"linux-x86_64","labels":{"rack":"r12"},"later":[1,2]}"#;
    let (dir, args) = rig("tierstore-facts-id", Some(facts), &[]);
    let start = resolve(args.clone()).unwrap();
    assert!(!start.check_only);
    assert_eq!(
        start.settings.into_storage().unwrap().proxy_id,
        "tierstore-web-04"
    );
    let mut check = args.clone();
    check.push("--check-config".into());
    let checked = resolve(check.clone()).unwrap();
    assert!(checked.check_only);
    assert_eq!(
        checked.settings.into_storage().unwrap().proxy_id,
        "tierstore-web-04"
    );
    std::fs::write(
        dir.path().join("facts.json"),
        r#"{"schema":1,"host_id":"web-05"}"#,
    )
    .unwrap();
    let again = resolve(check).unwrap();
    assert_eq!(
        again.settings.into_storage().unwrap().proxy_id,
        "tierstore-web-05"
    );
}

/// Asserts that a build node's file (the shape `[local_cache]` and `[cold_store]` make) takes its
/// id from the facts file too, and that an empty prefix makes the host id the id.
#[test]
fn a_build_nodes_file_takes_its_proxy_id_from_the_facts_file_and_the_prefix_may_be_empty() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-facts-cache");
    let facts = dir.path().join("facts.json");
    std::fs::write(&facts, r#"{"schema":1,"host_id":"mac-7"}"#).unwrap();
    let text = format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
         [server]\nproxy_id_from_facts = {{ path = {:?}, prefix = \"\" }}\n\
         bucket = \"b\"\naccess_key_file = \"key\"\n\n\
         [local_cache]\npath = \"/c\"\nbudget = 1000\n\n[cold_store]\nkind = \"memory\"\n\n\
         [[policy]]\nprefix = \"cas/\"\ncache = true\n",
        facts.display().to_string()
    );
    let file = dir.path().join("tierstore.toml");
    std::fs::write(&file, text).unwrap();
    let args = vec!["tierstore".into(), "--config".into(), file.into()];
    let cache = resolve(args).unwrap().settings.into_cache().unwrap();
    assert_eq!(cache.front.proxy_id, "mac-7");
}

/// Asserts that the loaders of text read no file: a path that does not exist is not looked at,
/// and the id is made of the prefix and a stand-in host id, so that the pages and the examples
/// that name `/var/lib/hostkeeper/facts.json` load.
#[test]
fn the_loaders_of_text_read_no_facts_file() {
    let text = with_id(
        "proxy_id_from_facts = { path = \"/nonexistent/facts.json\", prefix = \"tierstore-\" }",
    );
    assert!(!Path::new("/nonexistent/facts.json").exists());
    let cli = from_text(&text, DEFAULTS).unwrap();
    assert_eq!(cli.proxy_id, "tierstore-host");
    assert!(settings_from_text(&text, DEFAULTS).is_ok());
}

/// Asserts the refusals that are of the file, which no facts file is read for: both keys, neither
/// key, a path that is not absolute, a prefix that cannot make an id, and an inline table with a
/// key it does not have or without one it needs.
#[test]
fn the_file_is_refused_when_it_gives_both_keys_or_neither_or_a_bad_table() {
    let both =
        with_id("proxy_id = \"p1\"\nproxy_id_from_facts = { path = \"/f.json\", prefix = \"\" }");
    assert_eq!(
        settings_from_text(&both, DEFAULTS).unwrap_err().to_string(),
        "server.proxy_id_from_facts: give proxy_id or proxy_id_from_facts, not both"
    );
    assert_eq!(
        settings_from_text(&with_id(""), DEFAULTS)
            .unwrap_err()
            .to_string(),
        "server.proxy_id: it is needed; give proxy_id or proxy_id_from_facts"
    );
    let relative = with_id("proxy_id_from_facts = { path = \"facts.json\", prefix = \"\" }");
    assert_eq!(
        settings_from_text(&relative, DEFAULTS)
            .unwrap_err()
            .to_string(),
        "server.proxy_id_from_facts: path is not an absolute path"
    );
    let slash = with_id("proxy_id_from_facts = { path = \"/f.json\", prefix = \"a/\" }");
    assert_eq!(
        settings_from_text(&slash, DEFAULTS)
            .unwrap_err()
            .to_string(),
        "server.proxy_id_from_facts: the proxy id \"a/host\" has a \"/\", which an id may not"
    );
    for table in [
        "{ path = \"/f.json\", prefix = \"\", more = 1 }",
        "{ path = \"/f.json\" }",
        "{ prefix = \"\" }",
    ] {
        let text = with_id(&format!("proxy_id_from_facts = {table}"));
        assert!(
            settings_from_text(&text, DEFAULTS).is_err(),
            "{table} is refused"
        );
    }
}

/// Asserts that a start is refused, with the message of the case, when the facts file is missing
/// or unreadable (the message names the path and not the system's text), is not JSON, is not an
/// object, has no `schema` of the integer 1, has no `host_id` that is a non-empty string, or makes an
/// id with a `/`; and that the message holds the path, the field and the host id and nothing else
/// of the file.
#[test]
fn a_facts_file_that_cannot_make_an_id_refuses_the_start_with_its_message_and_none_of_its_content()
{
    let cases: [(Option<&str>, &str); 12] = [
        (None, "cannot be read: entity not found"),
        (Some("not json SECRET"), "is not JSON"),
        (Some("[\"SECRET\"]"), "is not a JSON object"),
        (Some("\"SECRET\""), "is not a JSON object"),
        (Some(r#"{"host_id":"h"}"#), "schema is not the integer 1"),
        (
            Some(r#"{"schema":2,"host_id":"h"}"#),
            "schema is not the integer 1",
        ),
        (
            Some(r#"{"schema":"1","host_id":"h"}"#),
            "schema is not the integer 1",
        ),
        (
            Some(r#"{"schema":1.0,"host_id":"h"}"#),
            "schema is not the integer 1",
        ),
        (Some(r#"{"schema":1}"#), "host_id is not a string"),
        (
            Some(r#"{"schema":1,"host_id":7,"x":"SECRET"}"#),
            "host_id is not a string",
        ),
        (Some(r#"{"schema":1,"host_id":""}"#), "host_id is empty"),
        (
            Some(r#"{"schema":1,"host_id":"a/b"}"#),
            "the proxy id \"tierstore-a/b\" has a \"/\", which an id may not",
        ),
    ];
    for (facts, what) in cases {
        for extra in [&[][..], &["--check-config"][..]] {
            let (dir, args) = rig("tierstore-facts-refused", facts, extra);
            let path = dir.path().join("facts.json").display().to_string();
            let config = args[2].to_string_lossy().into_owned();
            let expected = if what.starts_with("the proxy id") {
                format!("config file {config}: server.proxy_id_from_facts: {what}")
            } else {
                format!(
                    "config file {config}: server.proxy_id_from_facts: the facts file {path}: {what}"
                )
            };
            assert_eq!(refusal(args), expected);
        }
    }
}

/// Asserts that a facts file that is a directory is refused as unreadable, naming the path and
/// the kind of the fault only.
#[test]
fn a_facts_path_that_is_a_directory_is_refused_as_unreadable() {
    let (dir, args) = rig("tierstore-facts-dir", None, &[]);
    std::fs::create_dir(dir.path().join("facts.json")).unwrap();
    let path = dir.path().join("facts.json").display().to_string();
    let config = args[2].to_string_lossy().into_owned();
    assert_eq!(
        refusal(args),
        format!(
            "config file {config}: server.proxy_id_from_facts: the facts file {path}: cannot be read: is a directory"
        )
    );
}

/// Asserts that a file with both keys is refused for that and not for the facts file, which is
/// then not read (it does not exist).
#[test]
fn a_file_with_both_keys_is_refused_without_reading_the_facts_file() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-facts-both");
    let file = dir.path().join("tierstore.toml");
    let text = with_id(
        "proxy_id = \"p1\"\nproxy_id_from_facts = { path = \"/nonexistent/f.json\", prefix = \"\" }",
    );
    std::fs::write(&file, text).unwrap();
    let args = vec!["tierstore".into(), "--config".into(), file.clone().into()];
    assert_eq!(
        refusal(args),
        format!(
            "config file {}: server.proxy_id_from_facts: give proxy_id or proxy_id_from_facts, not both",
            file.display()
        )
    );
}

/// Asserts that a literal `proxy_id` is still the id, and that `proxy_id_from_facts` is a key
/// this release knows (no warning of an unknown key), where a misspelling is one.
#[test]
fn proxy_id_is_still_the_id_and_proxy_id_from_facts_is_a_known_key() {
    assert_eq!(from_text(MINIMAL, DEFAULTS).unwrap().proxy_id, "p1");
    assert_eq!(
        proxy_id_of(Some("x"), None, Some("host")).unwrap(),
        "x".to_owned()
    );
    let fields = fields_of::<ServerTable>();
    assert!(fields.contains(&"proxy_id_from_facts"));
    assert!(!fields.contains(&"id"));
    let known = with_id("proxy_id_from_facts = { path = \"/f.json\", prefix = \"\" }");
    let table: toml::Table = toml::from_str(&known).unwrap();
    assert_eq!(unknown_key_warnings(&table), Vec::<String>::new());
    let _: File = toml::from_str(&known).unwrap();
    let typo = known.replace("proxy_id_from_facts", "proxy_id_from_fact");
    let table: toml::Table = toml::from_str(&typo).unwrap();
    assert_eq!(
        unknown_key_warnings(&table),
        [
            "tierstore: config key server.proxy_id_from_fact is not known to this release and is ignored"
        ]
    );
}

/// Asserts which texts ask for a facts file: only one whose `server.proxy_id_from_facts` has an
/// absolute `path` and that gives no `proxy_id`; the rest is left to the read of the whole file.
#[test]
fn only_an_absolute_path_with_no_proxy_id_asks_for_a_facts_file() {
    let wants = |text: &str| facts_path(text).map(|path| path.display().to_string());
    let facts = "proxy_id_from_facts = { path = \"/f.json\", prefix = \"\" }";
    assert_eq!(
        wants(&format!("[server]\n{facts}\n")),
        Some("/f.json".to_owned())
    );
    assert_eq!(
        wants(&format!("[server]\nproxy_id = \"p\"\n{facts}\n")),
        None
    );
    assert_eq!(
        wants("[server]\nproxy_id_from_facts = { path = \"f.json\" }\n"),
        None
    );
    assert_eq!(wants("[server]\nproxy_id = \"p\"\n"), None);
    assert_eq!(wants("[hot_store]\nkind = \"memory\"\n"), None);
    assert_eq!(wants("not toml ["), None);
    assert_eq!(wants("[server]\nproxy_id_from_facts = 3\n"), None);
}
