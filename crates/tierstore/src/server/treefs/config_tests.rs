//! The `[fs]` table: defaults, every refusal, and the lookup of group and user names.

use std::path::PathBuf;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{Database, FsSettings, builders, from_text, ignored, load, load_with};

/// The keys every file needs.
const NEEDED: &str = "client_group = \"builders\"\nowner = \"1000:1001\"\n\
    cache_path = \"/var/cache/tierstore\"\n";

fn fs(extra: &str) -> String {
    format!("[fs]\n{NEEDED}{extra}")
}

fn refused(text: &str) -> String {
    from_text(text, &builders).unwrap_err().to_string()
}

/// Asserts that a file with only the needed keys gets the defaults of the table, and the group by
/// name from the lookup of group names.
#[test]
fn the_needed_keys_alone_give_the_defaults() {
    let got = from_text(&fs(""), &builders).unwrap();
    let want = FsSettings {
        mountpoint: PathBuf::from("/run/tierstore/fs"),
        runtime_dir: PathBuf::from("/run/tierstore-fs"),
        control_socket: PathBuf::from("/run/tierstore-fs/control.sock"),
        client_group: 2001,
        owner: (1000, 1001),
        cache_path: PathBuf::from("/var/cache/tierstore"),
        fetch_timeout: Duration::from_secs(60),
        threads: 16,
        handoff_socket: PathBuf::from("/run/tierstore-fs/handoff.sock"),
        handoff_wait: Duration::from_secs(120),
        recover_wait: Duration::from_secs(30),
        reattach_concurrency: 16,
        tree_idle: Duration::from_secs(900),
        tree_list_ram_bytes: 536_870_912,
        allow_other: true,
        nfs: crate::server::treefs::nfs::config::NfsSettings::default(),
        put: None,
    };
    assert_eq!(got, want);
}

/// Asserts that every key set in the file is taken, the group by number too, and the control socket
/// defaults to the runtime directory that was set.
#[test]
fn every_key_is_taken_when_it_is_set() {
    let text = "[fs]\nmountpoint = \"/m\"\nruntime_dir = \"/r\"\nclient_group = \"77\"\n\
        owner = \"5:6\"\ncache_path = \"/c\"\nfetch_timeout = \"5s\"\n\
        threads = 3\n\
        handoff_socket = \"/h.sock\"\nhandoff_wait = \"5s\"\nrecover_wait = \"2m\"\nreattach_concurrency = 3\n\
        tree_idle_secs = 604800\n";
    let got = from_text(text, &builders).unwrap();
    assert_eq!(got.mountpoint, PathBuf::from("/m"));
    assert_eq!(got.control_socket, PathBuf::from("/r/control.sock"));
    assert_eq!((got.client_group, got.owner), (77, (5, 6)));
    assert_eq!(
        (got.fetch_timeout, got.threads),
        (Duration::from_secs(5), 3)
    );
    assert_eq!(got.reattach_concurrency, 3);
    assert_eq!(got.tree_idle, Duration::from_secs(604_800));
    assert_eq!(got.handoff_socket, PathBuf::from("/h.sock"));
    assert_eq!(
        (got.handoff_wait, got.recover_wait),
        (Duration::from_secs(5), Duration::from_secs(120))
    );
    // Left out, the hand-over socket follows the runtime directory that was set.
    let text = "[fs]\nruntime_dir = \"/r\"\nclient_group = \"77\"\nowner = \"5:6\"\n\
        cache_path = \"/c\"\n";
    assert_eq!(
        from_text(text, &builders).unwrap().handoff_socket,
        PathBuf::from("/r/handoff.sock")
    );
    let own = fs("control_socket = \"/elsewhere.sock\"\n");
    assert_eq!(
        from_text(&own, &builders).unwrap().control_socket,
        PathBuf::from("/elsewhere.sock")
    );
}

/// Asserts that `fs.fill_socket` is not needed and is read and ignored: a file without it loads,
/// and a file with it, whatever its value (a relative path included), makes the very settings the
/// file without it makes.
#[test]
fn fill_socket_is_not_needed_and_is_ignored() {
    let without = from_text(&fs(""), &builders).unwrap();
    for value in ["/run/tierstore/fill.sock", "relative.sock"] {
        let text = fs(&format!("fill_socket = \"{value}\"\n"));
        assert_eq!(from_text(&text, &builders).unwrap(), without, "{value}");
    }
}

/// Asserts that a file that leaves out `fs.tree_list_ram_bytes` keeps 512 MiB of tree lists
/// mapped.
#[test]
fn tree_list_ram_bytes_defaults_to_512_mib() {
    let got = from_text(&fs(""), &builders).unwrap();
    assert_eq!(got.tree_list_ram_bytes, 512 * 1024 * 1024);
}

/// Asserts that `fs.tree_list_ram_bytes` is read as it is set: 2 GiB, and a page, the least.
#[test]
fn tree_list_ram_bytes_reads_2_gib() {
    let two = from_text(&fs("tree_list_ram_bytes = 2147483648\n"), &builders).unwrap();
    assert_eq!(two.tree_list_ram_bytes, 2 << 30);
    let page = from_text(&fs("tree_list_ram_bytes = 4096\n"), &builders).unwrap();
    assert_eq!(page.tree_list_ram_bytes, 4096);
}

/// Asserts that `fs.tree_list_ram_bytes` under a page is refused at start, by name, and nothing is
/// taken of the file.
#[test]
fn tree_list_ram_bytes_under_a_page_is_refused_by_name() {
    for (value, want) in [
        (
            "4095",
            "fs.tree_list_ram_bytes: 4095 is not between 4096 and 18446744073709551615",
        ),
        (
            "0",
            "fs.tree_list_ram_bytes: 0 is not between 4096 and 18446744073709551615",
        ),
    ] {
        let text = fs(&format!("tree_list_ram_bytes = {value}\n"));
        assert_eq!(refused(&text), want, "{value}");
    }
}

/// Asserts each refusal and that it names the key.
#[test]
fn every_refusal_names_its_key() {
    let cases: Vec<(String, &str)> = vec![
        (fs("mountpoint = \"relative\"\n"), "fs.mountpoint"),
        (fs("runtime_dir = \"relative\"\n"), "fs.runtime_dir"),
        (
            fs("control_socket = \"relative.sock\"\n"),
            "fs.control_socket",
        ),
        (
            fs(&format!("control_socket = \"/{}\"\n", "s".repeat(120))),
            "fs.control_socket",
        ),
        (
            fs("handoff_socket = \"relative.sock\"\n"),
            "fs.handoff_socket",
        ),
        (
            fs(&format!("handoff_socket = \"/{}\"\n", "s".repeat(120))),
            "fs.handoff_socket",
        ),
        (fs("handoff_wait = \"999ms\"\n"), "fs.handoff_wait"),
        (fs("handoff_wait = \"2h\"\n"), "fs.handoff_wait"),
        (fs("reattach_concurrency = 0\n"), "fs.reattach_concurrency"),
        (
            fs("reattach_concurrency = 1025\n"),
            "fs.reattach_concurrency",
        ),
        (fs("tree_idle_secs = 0\n"), "fs.tree_idle_secs"),
        (fs("tree_idle_secs = 604801\n"), "fs.tree_idle_secs"),
        (fs("recover_wait = \"0s\"\n"), "fs.recover_wait"),
        (fs("recover_wait = \"3h\"\n"), "fs.recover_wait"),
        (fs("fetch_timeout = \"0ms\"\n"), "fs.fetch_timeout"),
        (fs("fetch_timeout = \"2h\"\n"), "fs.fetch_timeout"),
        (fs("threads = 0\n"), "fs.threads"),
        (fs("threads = 253\n"), "fs.threads"),
        (
            "[fs]\nowner = \"1:2\"\ncache_path = \"/c\"\n".to_owned(),
            "fs.client_group",
        ),
        (
            "[fs]\nclient_group = \"1\"\ncache_path = \"/c\"\n".to_owned(),
            "fs.owner",
        ),
        (
            "[fs]\nclient_group = \"1\"\nowner = \"1:2\"\n".to_owned(),
            "fs.cache_path",
        ),
        (
            "[fs]\nclient_group = \"1\"\nowner = \"1:2\"\ncache_path = \"c\"\n".to_owned(),
            "fs.cache_path",
        ),
        (fs("").replace("1000:1001", "1000"), "fs.owner"),
        (fs("").replace("1000:1001", "a:b"), "fs.owner"),
        (fs("").replace("1000:1001", "1:-2"), "fs.owner"),
        (fs("").replace("builders", "nobody-here"), "fs.client_group"),
    ];
    for (text, key) in cases {
        let why = refused(&text);
        assert!(why.starts_with(key), "{key}: {why}");
    }
}

/// Asserts that a file that is not TOML, has no `[fs]` table or a value of the wrong type is a
/// syntax error that shows no value.
#[test]
fn a_file_that_is_not_the_table_is_a_syntax_error_without_values() {
    assert!(refused("not toml [").contains("line 1"));
    assert!(refused("[server]\nid = 1\n").contains("fs"));
    let why = refused("[fs]\nthreads = \"seventeen\"\n");
    assert!(!why.contains("seventeen"), "{why}");
    // A switch is true or false, nothing else.
    let text = format!("{}[fs.put]\nenabled = \"yes\"\n", fs(""));
    assert!(refused(&text).contains("expected a boolean"));
}

/// Asserts that a key this release does not know is ignored and the file loads.
#[test]
fn a_key_of_a_later_release_is_ignored() {
    let text = fs("future_knob = 3\n");
    assert!(from_text(&text, &builders).is_ok());
    assert_eq!(
        ignored("future_knob"),
        "tierstore fs: fs.future_knob is not a key of this release and is ignored"
    );
}

/// Asserts that a file written for a release that had passthrough starts with its keys, whatever
/// their values (a value the release refused included), with the settings it would have without
/// them, and that the warning of each names the release that removed it and why.
#[test]
fn the_keys_of_passthrough_are_ignored_with_a_warning_that_names_the_release() {
    let plain = from_text(&fs(""), &builders).unwrap();
    for removed in [
        "passthrough = false\nmax_backing_ids = 7\n",
        "passthrough = \"yes\"\nmax_backing_ids = 0\n",
    ] {
        assert_eq!(
            from_text(&fs(removed), &builders).unwrap(),
            plain,
            "{removed}"
        );
    }
    assert_eq!(
        ignored("passthrough"),
        "tierstore fs: fs.passthrough is ignored: removed after tierstore 0.3.121, with \
         passthrough: every open is a cached open"
    );
    assert_eq!(
        ignored("max_backing_ids"),
        "tierstore fs: fs.max_backing_ids is ignored: removed after tierstore 0.3.121, with \
         passthrough: no backing id is registered"
    );
}

/// Asserts that a file is read from a path, a missing one is an error naming it, a group by number
/// asks no lookup, and a group by name is the one the lookup given finds.
#[test]
fn a_file_is_read_from_its_path() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-config");
    let path = dir.path().join("fs.toml");
    std::fs::write(&path, fs("").replace("builders", "4242")).unwrap();
    assert_eq!(load(&path).unwrap().client_group, 4242);
    let missing = load(&dir.path().join("none.toml")).unwrap_err().to_string();
    assert!(missing.contains("none.toml"), "{missing}");
    std::fs::write(&path, fs("")).unwrap();
    assert_eq!(load_with(&path, &builders).unwrap().client_group, 2001);
}

/// Asserts that a group given by name is the id the system's lookup gives for it, though no group
/// file lists it (a Mac keeps its service account's group in the directory service alone, which
/// the group file of an earlier release did not see); that a name the lookup does not find, and a
/// lookup that fails, are refusals of `fs.client_group` with their exact words; and that a group
/// given by number asks the lookup nothing. Catches a name read from a group file only.
#[test]
fn a_group_name_is_looked_up_through_the_system() {
    let directory = |_: Database, name: &str| Ok((name == "_tierstore").then_some(394));
    let text = fs("").replace("builders", "_tierstore");
    assert_eq!(from_text(&text, &directory).unwrap().client_group, 394);

    let absent = from_text(&fs(""), &directory).unwrap_err().to_string();
    assert_eq!(
        absent,
        "fs.client_group: \"builders\" is neither a group id nor the name of a group of this system"
    );
    let down = |_: Database, _: &str| Err("/usr/bin/getent could not be run: gone".to_owned());
    assert_eq!(
        from_text(&fs(""), &down).unwrap_err().to_string(),
        "fs.client_group: the group builders could not be looked up: /usr/bin/getent could not \
         be run: gone"
    );
    let asked = std::cell::Cell::new(0);
    let counting = |_: Database, _: &str| {
        asked.set(asked.get() + 1);
        Ok(None)
    };
    let by_number = fs("").replace("builders", "77");
    assert_eq!(from_text(&by_number, &counting).unwrap().client_group, 77);
    assert_eq!(asked.get(), 0);
}

/// Asserts that `fs.threads` takes the most readers a `tsfuse` session holds, and that the bound
/// is that library's: each thread reads its own copy of the connection's descriptor.
#[test]
fn the_most_threads_are_the_descriptors_a_session_holds() {
    let got = from_text(&fs("threads = 252\n"), &builders).unwrap();
    assert_eq!(got.threads, 252);
    #[cfg(target_os = "linux")]
    assert_eq!(super::MOST_THREADS as usize, tsfuse::MAX_DEVICES);
}

/// A lookup of a system with the user `_tierstore` (id 394) and the group `_tierstore` (id 395),
/// and no other name.
fn service_account(database: Database, name: &str) -> super::Found {
    Ok((name == "_tierstore").then_some(match database {
        Database::User => 394,
        Database::Group => 395,
    }))
}

/// The needed keys with `owner` as the value of `fs.owner`, and the client group by number, so that
/// only the owner is looked up.
fn owned_by(owner: &str) -> String {
    fs("")
        .replace("1000:1001", owner)
        .replace("client_group = \"builders\"", "client_group = \"77\"")
}

/// Asserts that `fs.owner` takes a user and a group by name, each from its own database, and a
/// number on either side: `name:name`, `number:name`, `name:number` and `number:number`. Catches a
/// shipped file that needs the numeric id of the service account of each Mac, and a user name
/// looked up as a group.
#[test]
fn an_owner_is_a_user_and_a_group_by_name_or_number() {
    let owner = |text: &str| from_text(&owned_by(text), &service_account).unwrap().owner;
    assert_eq!(owner("_tierstore:_tierstore"), (394, 395));
    assert_eq!(owner("7:_tierstore"), (7, 395));
    assert_eq!(owner("_tierstore:8"), (394, 8));
    assert_eq!(owner("0:0"), (0, 0));
}

/// Asserts that an owner given by two numbers asks the lookup nothing, and that one by name asks
/// the user database for the user and the group database for the group, each once.
#[test]
fn an_owner_by_number_asks_nothing_and_one_by_name_asks_each_database() {
    let asked = std::cell::RefCell::new(Vec::new());
    let recording = |database: Database, name: &str| {
        asked.borrow_mut().push((database, name.to_owned()));
        Ok(Some(5))
    };
    let numbers = from_text(&owned_by("12:34"), &recording).unwrap();
    assert_eq!(numbers.owner, (12, 34));
    assert!(asked.borrow().is_empty());
    let names = from_text(&owned_by("alice:staff"), &recording).unwrap();
    assert_eq!(names.owner, (5, 5));
    assert_eq!(
        *asked.borrow(),
        [
            (Database::User, "alice".to_owned()),
            (Database::Group, "staff".to_owned())
        ]
    );
}

/// Asserts the refusal of each wrong `fs.owner`, by its exact words: not two parts (no colon, an
/// empty side, three parts, nothing), a user that is not one, a group that is not one, a lookup
/// that failed (with what the tool said), and an id too large for a number. Catches a typo'd name
/// taken as some default owner.
#[test]
fn a_wrong_owner_is_refused_with_its_key_and_what_was_wrong() {
    let not_two = |text: &str| {
        format!(
            "fs.owner: must be user:group, two parts joined by one colon, each a number or a name \
             of this system, not {text:?}"
        )
    };
    for text in ["1000", ":1", "1:", ":", "1:2:3", ""] {
        let why = from_text(&owned_by(text), &service_account).unwrap_err();
        assert_eq!(why.to_string(), not_two(text), "{text:?}");
    }
    let refuse = |text: &str, lookup: super::Lookup<'_>| {
        from_text(&owned_by(text), lookup).unwrap_err().to_string()
    };
    assert_eq!(
        refuse("typo:_tierstore", &service_account),
        "fs.owner: \"typo\" is neither a user id nor the name of a user of this system"
    );
    assert_eq!(
        refuse("_tierstore:typo", &service_account),
        "fs.owner: \"typo\" is neither a group id nor the name of a group of this system"
    );
    let down = |database: Database, _: &str| match database {
        Database::User => Ok(Some(1)),
        Database::Group => Err("/usr/bin/getent could not be run: gone".to_owned()),
    };
    assert_eq!(
        refuse("a:b", &down),
        "fs.owner: the group b could not be looked up: /usr/bin/getent could not be run: gone"
    );
    let all_down = |_: Database, _: &str| Err("dscacheutil failed (exit status: 1): x".to_owned());
    assert_eq!(
        refuse("a:1", &all_down),
        "fs.owner: the user a could not be looked up: dscacheutil failed (exit status: 1): x"
    );
    assert_eq!(
        refuse("99999999999:1", &service_account),
        "fs.owner: the user id 99999999999 is too large"
    );
    assert_eq!(
        refuse("1:99999999999", &service_account),
        "fs.owner: the group id 99999999999 is too large"
    );
}

/// Asserts that an empty `fs.client_group` is a name to look up, which the system does not have,
/// not a number: it is refused naming the key.
#[test]
fn an_empty_client_group_is_refused() {
    let text = fs("").replace("\"builders\"", "\"\"");
    let why = from_text(&text, &builders).unwrap_err().to_string();
    assert_eq!(
        why,
        "fs.client_group: \"\" is neither a group id nor the name of a group of this system"
    );
}
