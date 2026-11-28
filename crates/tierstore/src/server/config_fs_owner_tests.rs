//! `params.owner = { subids_of, id }` of an `fs` interface: the ids it resolves to over fixture
//! subordinate id files (no file of the host is read), and each refusal by its exact message.

use std::io;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{Host, SUBUID, first_range, read_text};
use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_testing::DEFAULTS;
use crate::server::treefs::{Database, Found};

/// The users (and groups) of the fixture system: `builders` is also the client group.
fn users(_database: Database, name: &str) -> Found {
    Ok(match name {
        "builders" => Some(2001),
        "small" => Some(2002),
        "high" => Some(2003),
        "byuid" => Some(2004),
        "nogroup" => Some(2005),
        "huge" => Some(2006),
        _ => None,
    })
}

/// The fixture `/etc/subuid`: for `builders` a range too small, two lines that are no range, the
/// first range of 65536 and a later one; `byuid` named by its uid; `high` at the top of the ids;
/// `small` with only 65535 ids; `huge` past the ids altogether.
const SUBUIDS: &str = "other:100000:65536
builders:200000:1000
builders:nope:65536
builders:1:many
builders:250000:65536:more
builders:300000:65536
builders:400000:65536
2004:500000:65536
high:4294901760:65536
small:600000:65535
nogroup:700000:65536
huge:4294967296:65536
";

/// The fixture `/etc/subgid`, with no line of `nogroup`.
const SUBGIDS: &str = "builders:310000:65536
2004:510000:65536
high:4294901760:65536
small:610000:65535
";

/// The fixture files.
fn fixture(path: &Path) -> io::Result<String> {
    Ok(if path == Path::new(SUBUID) {
        SUBUIDS
    } else {
        SUBGIDS
    }
    .to_owned())
}

/// A system whose subordinate id files cannot be read.
fn unreadable(_path: &Path) -> io::Result<String> {
    Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
}

/// A system whose `/etc/subuid` is the fixture's and whose `/etc/subgid` is not there.
fn no_subgid(path: &Path) -> io::Result<String> {
    if path == Path::new(SUBUID) {
        return fixture(path);
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "not there"))
}

/// Every path on one file system.
fn one_volume(_path: &Path) -> io::Result<u64> {
    Ok(7)
}

/// A build node's file whose `fs` interface has `owner` as its owner.
fn file(owner: &str) -> String {
    format!(
        r#"
[[interface]]
type   = "s3"
params = {{ port = 9300 }}

[[interface]]
type = "fs"
[interface.params]
client_group = "builders"
owner        = {owner}

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#
    )
}

/// The owner the file with `owner` loads with, the files read by `read`, or its refusal.
fn loaded_with(owner: &str, read: fn(&Path) -> io::Result<String>) -> Result<(u32, u32), String> {
    let run = PerRun {
        host: Host {
            names: users,
            volume: one_volume,
            read,
        },
        ..DEFAULTS
    };
    let settings = settings_from_text(&file(owner), run).map_err(|why| why.to_string())?;
    let fs = settings.into_cache().expect("a build node").fs;
    Ok(fs.expect("an fs interface").owner)
}

/// The owner of the file with `owner` over the fixture files, or its refusal.
fn loaded(owner: &str) -> Result<(u32, u32), String> {
    loaded_with(owner, fixture)
}

/// `{ subids_of = <user>, id = <id> }`.
fn subids(user: &str, id: u64) -> String {
    format!("{{ subids_of = \"{user}\", id = {id} }}")
}

/// Asserts that the owner is the start of the user's first range of at least 65536 ids, in file
/// order, plus `id`, in `/etc/subuid` for the uid and `/etc/subgid` for the gid: a smaller range
/// before it, a line that is not three parts or whose start or count is not a number, another
/// user's range, and a later range of the user are passed over.
#[test]
fn the_owner_is_the_first_range_of_at_least_65536_plus_the_id() {
    assert_eq!(loaded(&subids("builders", 1000)), Ok((301_000, 311_000)));
    assert_eq!(loaded(&subids("builders", 0)), Ok((300_000, 310_000)));
    assert_eq!(loaded(&subids("builders", 65535)), Ok((365_535, 375_535)));
}

/// Asserts that a line that names the user by its uid is the user's, whether the table names the
/// user by name or by number.
#[test]
fn a_range_named_by_uid_is_found() {
    assert_eq!(loaded(&subids("byuid", 1000)), Ok((501_000, 511_000)));
    assert_eq!(loaded(&subids("2004", 1000)), Ok((501_000, 511_000)));
    assert_eq!(
        first_range("2004:5:65536\n", "byuid", 2004),
        Some((5, 65536))
    );
    assert_eq!(first_range("2004:5:65536\n", "byuid", 2005), None);
}

/// Asserts that a user whose ranges in a file all hold fewer than 65536 ids, a user with no line
/// in `/etc/subgid`, and a user the system does not have are each refused by name, naming the key
/// and the file, and not given some other owner.
#[test]
fn a_user_without_such_a_range_is_refused_by_name() {
    assert_eq!(
        loaded(&subids("small", 1000)),
        Err(
            "interface[1].params.owner: the user \"small\" (uid 2002) has no range of at least \
             65536 ids in /etc/subuid"
                .to_owned()
        )
    );
    assert_eq!(
        loaded(&subids("nogroup", 1000)),
        Err(
            "interface[1].params.owner: the user \"nogroup\" (uid 2005) has no range of at least \
             65536 ids in /etc/subgid"
                .to_owned()
        )
    );
    assert_eq!(
        loaded(&subids("typo", 1000)),
        Err(
            "interface[1].params.owner: \"typo\" is neither a user id nor the name of a user of \
             this system"
                .to_owned()
        )
    );
}

/// Asserts that an owner past 4294967294 (4294967295 is no id), or past the ids altogether, is
/// refused naming the sum, and that 4294967294 itself is taken.
#[test]
fn ids_past_4294967294_are_refused() {
    assert_eq!(
        loaded(&subids("high", 65534)),
        Ok((4_294_967_294, 4_294_967_294))
    );
    assert_eq!(
        loaded(&subids("high", 65535)),
        Err(
            "interface[1].params.owner: 4294901760 + 65535 in /etc/subuid is 4294967295, past \
             4294967294, the largest id of a user or a group"
                .to_owned()
        )
    );
    assert_eq!(
        loaded(&subids("huge", 0)),
        Err(
            "interface[1].params.owner: 4294967296 + 0 in /etc/subuid is 4294967296, past \
             4294967294, the largest id of a user or a group"
                .to_owned()
        )
    );
    assert_eq!(
        first_range("u:18446744073709551615:65536\n", "u", 1),
        Some((u64::MAX, 65536))
    );
}

/// Asserts that an `id` that is not inside the user's range, which would be another user's ids,
/// is refused, the largest id TOML can say too.
#[test]
fn an_id_outside_the_users_range_is_refused() {
    assert_eq!(
        loaded(&subids("builders", 65536)),
        Err(
            "interface[1].params.owner: id 65536 is outside the range of 65536 ids of the user \
             \"builders\" in /etc/subuid, which starts at 300000"
                .to_owned()
        )
    );
    let most = loaded(&subids("builders", 9_223_372_036_854_775_807)).expect_err("refused");
    assert!(
        most.contains("id 9223372036854775807 is outside the range"),
        "{most}"
    );
}

/// Asserts that a subordinate id file that cannot be read is refused naming the file and what
/// the system said, `/etc/subgid` after a `/etc/subuid` that was read.
#[test]
fn a_subid_file_that_cannot_be_read_is_refused_naming_it() {
    assert_eq!(
        loaded_with(&subids("builders", 1000), unreadable),
        Err("interface[1].params.owner: /etc/subuid could not be read: denied".to_owned())
    );
    assert_eq!(
        loaded_with(&subids("builders", 1000), no_subgid),
        Err("interface[1].params.owner: /etc/subgid could not be read: not there".to_owned())
    );
}

/// Asserts that `user:group` keeps its meaning beside the table, and that a table with a key it
/// does not have, one without `id` or `subids_of`, a negative `id`, and an owner that is neither a
/// string nor a table are refused at load.
#[test]
fn a_wrong_owner_table_is_refused() {
    assert_eq!(loaded("\"7:builders\""), Ok((7, 2001)));
    let refused = |owner: &str| loaded(owner).expect_err("refused");
    let cases = [
        (
            "{ subids_of = \"builders\", id = 1, extra = 2 }",
            "unknown field `extra`",
        ),
        ("{ subids_of = \"builders\" }", "missing field `id`"),
        ("{ id = 1000 }", "missing field `subids_of`"),
        (
            "{ subids_of = \"builders\", id = -1 }",
            "not a value this key takes (it is not shown); expected u64",
        ),
        (
            "5",
            "expected \"user:group\" or a table { subids_of, id }, found integer",
        ),
    ];
    for (owner, said) in cases {
        let why = refused(owner);
        assert!(why.contains(said), "{owner}: {why}");
    }
}

/// Asserts that the table is the `fs` interface's alone: another type refuses `owner` in either
/// form.
#[test]
fn the_table_is_the_fs_interfaces_alone() {
    let text = file("\"1:2\"").replace(
        "params = { port = 9300 }",
        &format!(
            "params = {{ port = 9300, owner = {} }}",
            subids("builders", 1)
        ),
    );
    let run = PerRun {
        host: Host {
            names: users,
            volume: one_volume,
            read: fixture,
        },
        ..DEFAULTS
    };
    let why = settings_from_text(&text, run)
        .expect_err("refused")
        .to_string();
    assert_eq!(why, "interface[0].params.owner: not for type \"s3\"");
}

/// Asserts that the system's host reads a file's text as it is, and says why one cannot be read.
#[test]
fn the_system_host_reads_the_file_it_is_given() {
    let dir = TestDir::under(std::env::temp_dir(), "tsconfig-fs-owner");
    let path = dir.path().join("subuid");
    std::fs::write(&path, SUBUIDS).expect("a file");
    assert_eq!((Host::SYSTEM.read)(&path).ok().as_deref(), Some(SUBUIDS));
    assert_eq!(
        (PerRun::default().host.read)(&path).ok().as_deref(),
        Some(SUBUIDS)
    );
    let gone = read_text(&dir.path().join("gone")).expect_err("not there");
    assert_eq!(gone.kind(), io::ErrorKind::NotFound);
}
