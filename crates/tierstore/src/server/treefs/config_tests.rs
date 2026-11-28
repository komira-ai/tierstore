//! The params of an `fs` interface: defaults, every refusal, and the lookup of group and user
//! names.

use std::path::PathBuf;
use std::time::Duration;

use super::{Database, FsSettings, TEST_PLACE, builders, from_params};

/// The params every `fs` interface needs.
const NEEDED: &str = "client_group = \"builders\"\nowner = \"1000:1001\"\n";

fn fs(extra: &str) -> String {
    format!("{NEEDED}{extra}")
}

fn refused(text: &str) -> String {
    from_params(text, &builders).unwrap_err().to_string()
}

/// The name of the param `name` as a refusal says it.
fn at(name: &str) -> String {
    format!("{TEST_PLACE}.{name}")
}

/// Asserts that params with only the needed ones get the defaults, the node's cache directory, and
/// the group by name from the lookup of group names.
#[test]
fn the_needed_keys_alone_give_the_defaults() {
    let got = from_params(&fs(""), &builders).unwrap();
    let want = FsSettings {
        mountpoint: PathBuf::from("/run/tierstore/fs"),
        runtime_dir: PathBuf::from("/run/tierstore-fs"),
        control_socket: PathBuf::from("/run/tierstore-fs/control.sock"),
        client_group: 2001,
        owner: (1000, 1001),
        cache_path: PathBuf::from("/var/cache/tierstore"),
        fetch_timeout: Duration::from_secs(60),
        threads: 16,
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

/// Asserts that every param set is taken, the group by number too, and that the control socket
/// defaults to the runtime directory that was set.
#[test]
fn every_key_is_taken_when_it_is_set() {
    let text = "mountpoint = \"/m\"\nruntime_dir = \"/r\"\nclient_group = \"77\"\n\
        owner = \"5:6\"\nfetch_timeout = \"5s\"\nthreads = 3\n\
        handoff_wait = \"5s\"\nrecover_wait = \"2m\"\nreattach_concurrency = 3\n\
        tree_idle_secs = 604800\n";
    let got = from_params(text, &builders).unwrap();
    assert_eq!(got.mountpoint, PathBuf::from("/m"));
    assert_eq!(got.control_socket, PathBuf::from("/r/control.sock"));
    assert_eq!((got.client_group, got.owner), (77, (5, 6)));
    assert_eq!(
        (got.fetch_timeout, got.threads),
        (Duration::from_secs(5), 3)
    );
    assert_eq!(got.reattach_concurrency, 3);
    assert_eq!(got.tree_idle, Duration::from_secs(604_800));
    assert_eq!(
        (got.handoff_wait, got.recover_wait),
        (Duration::from_secs(5), Duration::from_secs(120))
    );
    let own = fs("control_socket = \"/elsewhere.sock\"\n");
    assert_eq!(
        from_params(&own, &builders).unwrap().control_socket,
        PathBuf::from("/elsewhere.sock")
    );
}

/// Asserts that params that leave out `tree_list_ram_bytes` keep 512 MiB of tree lists
/// mapped.
#[test]
fn tree_list_ram_bytes_defaults_to_512_mib() {
    let got = from_params(&fs(""), &builders).unwrap();
    assert_eq!(got.tree_list_ram_bytes, 512 * 1024 * 1024);
}

/// Asserts that `tree_list_ram_bytes` is read as it is set: 2 GiB, and a page, the least.
#[test]
fn tree_list_ram_bytes_reads_2_gib() {
    let two = from_params(&fs("tree_list_ram_bytes = 2147483648\n"), &builders).unwrap();
    assert_eq!(two.tree_list_ram_bytes, 2 << 30);
    let page = from_params(&fs("tree_list_ram_bytes = 4096\n"), &builders).unwrap();
    assert_eq!(page.tree_list_ram_bytes, 4096);
}

/// Asserts that `tree_list_ram_bytes` under a page is refused at start, by name, and nothing is
/// taken of the file.
#[test]
fn tree_list_ram_bytes_under_a_page_is_refused_by_name() {
    for (value, want) in [
        ("4095", "4095 is not between 4096 and 18446744073709551615"),
        ("0", "0 is not between 4096 and 18446744073709551615"),
    ] {
        let text = fs(&format!("tree_list_ram_bytes = {value}\n"));
        let want = format!("{}: {want}", at("tree_list_ram_bytes"));
        assert_eq!(refused(&text), want, "{value}");
    }
}

/// Asserts each refusal and that it names the param.
#[test]
fn every_refusal_names_its_key() {
    let long = "s".repeat(120);
    let cases: Vec<(String, &str)> = vec![
        (fs("mountpoint = \"relative\"\n"), "mountpoint"),
        (fs("runtime_dir = \"relative\"\n"), "runtime_dir"),
        (fs("control_socket = \"relative.sock\"\n"), "control_socket"),
        (
            fs(&format!("control_socket = \"/{long}\"\n")),
            "control_socket",
        ),
        (fs("handoff_wait = \"999ms\"\n"), "handoff_wait"),
        (fs("handoff_wait = \"2h\"\n"), "handoff_wait"),
        (fs("reattach_concurrency = 0\n"), "reattach_concurrency"),
        (fs("reattach_concurrency = 1025\n"), "reattach_concurrency"),
        (fs("tree_idle_secs = 0\n"), "tree_idle_secs"),
        (fs("tree_idle_secs = 604801\n"), "tree_idle_secs"),
        (fs("recover_wait = \"0s\"\n"), "recover_wait"),
        (fs("recover_wait = \"3h\"\n"), "recover_wait"),
        (fs("fetch_timeout = \"0ms\"\n"), "fetch_timeout"),
        (fs("fetch_timeout = \"2h\"\n"), "fetch_timeout"),
        (fs("threads = 0\n"), "threads"),
        (fs("threads = 253\n"), "threads"),
        ("owner = \"1:2\"\n".to_owned(), "client_group"),
        ("client_group = \"1\"\n".to_owned(), "owner"),
        (fs("").replace("1000:1001", "1000"), "owner"),
        (fs("").replace("1000:1001", "a:b"), "owner"),
        (fs("").replace("1000:1001", "1:-2"), "owner"),
        (fs("").replace("builders", "nobody-here"), "client_group"),
    ];
    for (text, name) in cases {
        let why = refused(&text);
        assert!(why.starts_with(&at(name)), "{name}: {why}");
    }
}

/// Asserts that a group given by name is the id the system's lookup gives for it, though no group
/// file lists it (a Mac keeps its service account's group in the directory service alone, which
/// the group file of an earlier release did not see); that a name the lookup does not find, and a
/// lookup that fails, are refusals of `client_group` with their exact words; and that a group
/// given by number asks the lookup nothing. Catches a name read from a group file only.
#[test]
fn a_group_name_is_looked_up_through_the_system() {
    let directory = |_: Database, name: &str| Ok((name == "_tierstore").then_some(394));
    let text = fs("").replace("builders", "_tierstore");
    assert_eq!(from_params(&text, &directory).unwrap().client_group, 394);

    let absent = from_params(&fs(""), &directory).unwrap_err().to_string();
    assert_eq!(
        absent,
        format!(
            "{}: \"builders\" is neither a group id nor the name of a group of this system",
            at("client_group")
        )
    );
    let down = |_: Database, _: &str| Err("/usr/bin/getent could not be run: gone".to_owned());
    assert_eq!(
        from_params(&fs(""), &down).unwrap_err().to_string(),
        format!(
            "{}: the group builders could not be looked up: /usr/bin/getent could not be run: gone",
            at("client_group")
        )
    );
    let asked = std::cell::Cell::new(0);
    let counting = |_: Database, _: &str| {
        asked.set(asked.get() + 1);
        Ok(None)
    };
    let by_number = fs("").replace("builders", "77");
    assert_eq!(from_params(&by_number, &counting).unwrap().client_group, 77);
    assert_eq!(asked.get(), 0);
}

/// Asserts that `threads` takes the most readers a `tsfuse` session holds, and that the bound
/// is that library's: each thread reads its own copy of the connection's descriptor.
#[test]
fn the_most_threads_are_the_descriptors_a_session_holds() {
    let got = from_params(&fs("threads = 252\n"), &builders).unwrap();
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

/// The needed keys with `owner` as the value of `owner`, and the client group by number, so that
/// only the owner is looked up.
fn owned_by(owner: &str) -> String {
    fs("")
        .replace("1000:1001", owner)
        .replace("client_group = \"builders\"", "client_group = \"77\"")
}

/// Asserts that `owner` takes a user and a group by name, each from its own database, and a
/// number on either side: `name:name`, `number:name`, `name:number` and `number:number`. Catches a
/// shipped file that needs the numeric id of the service account of each Mac, and a user name
/// looked up as a group.
#[test]
fn an_owner_is_a_user_and_a_group_by_name_or_number() {
    let owner = |text: &str| {
        from_params(&owned_by(text), &service_account)
            .unwrap()
            .owner
    };
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
    let numbers = from_params(&owned_by("12:34"), &recording).unwrap();
    assert_eq!(numbers.owner, (12, 34));
    assert!(asked.borrow().is_empty());
    let names = from_params(&owned_by("alice:staff"), &recording).unwrap();
    assert_eq!(names.owner, (5, 5));
    assert_eq!(
        *asked.borrow(),
        [
            (Database::User, "alice".to_owned()),
            (Database::Group, "staff".to_owned())
        ]
    );
}

/// Asserts the refusal of each wrong `owner`, by its exact words: not two parts (no colon, an
/// empty side, three parts, nothing), a user that is not one, a group that is not one, a lookup
/// that failed (with what the tool said), and an id too large for a number. Catches a typo'd name
/// taken as some default owner.
#[test]
fn a_wrong_owner_is_refused_with_its_key_and_what_was_wrong() {
    let not_two = |text: &str| {
        format!(
            "{}: must be user:group, two parts joined by one colon, each a number or a name of \
             this system, not {text:?}",
            at("owner")
        )
    };
    for text in ["1000", ":1", "1:", ":", "1:2:3", ""] {
        let why = from_params(&owned_by(text), &service_account).unwrap_err();
        assert_eq!(why.to_string(), not_two(text), "{text:?}");
    }
    let refuse = |text: &str, lookup: super::Lookup<'_>| {
        from_params(&owned_by(text), lookup)
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        refuse("typo:_tierstore", &service_account),
        format!(
            "{}: \"typo\" is neither a user id nor the name of a user of this system",
            at("owner")
        )
    );
    assert_eq!(
        refuse("_tierstore:typo", &service_account),
        format!(
            "{}: \"typo\" is neither a group id nor the name of a group of this system",
            at("owner")
        )
    );
    let down = |database: Database, _: &str| match database {
        Database::User => Ok(Some(1)),
        Database::Group => Err("/usr/bin/getent could not be run: gone".to_owned()),
    };
    assert_eq!(
        refuse("a:b", &down),
        format!(
            "{}: the group b could not be looked up: /usr/bin/getent could not be run: gone",
            at("owner")
        )
    );
    let all_down = |_: Database, _: &str| Err("dscacheutil failed (exit status: 1): x".to_owned());
    assert_eq!(
        refuse("a:1", &all_down),
        format!(
            "{}: the user a could not be looked up: dscacheutil failed (exit status: 1): x",
            at("owner")
        )
    );
    assert_eq!(
        refuse("99999999999:1", &service_account),
        format!("{}: the user id 99999999999 is too large", at("owner"))
    );
    assert_eq!(
        refuse("1:99999999999", &service_account),
        format!("{}: the group id 99999999999 is too large", at("owner"))
    );
}

/// Asserts that an empty `client_group` is a name to look up, which the system does not have,
/// not a number: it is refused naming the key.
#[test]
fn an_empty_client_group_is_refused() {
    let text = fs("").replace("\"builders\"", "\"\"");
    let why = from_params(&text, &builders).unwrap_err().to_string();
    assert_eq!(
        why,
        format!(
            "{}: \"\" is neither a group id nor the name of a group of this system",
            at("client_group")
        )
    );
}
