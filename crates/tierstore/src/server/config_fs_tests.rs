//! The `fs` interface of a file: the settings it makes, each rule the one file adds to `[fs]` by
//! its exact message, and the lookups of the system it makes at load.

use std::io;
use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::{Host, not_served, volume_of};
use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_testing::DEFAULTS;
use crate::server::treefs::{self, FsSettings, builders};

/// The file system every path of the tests is on.
const ONE: u64 = 7;

/// Every path on one file system.
fn one_volume(_path: &Path) -> io::Result<u64> {
    Ok(ONE)
}

/// Every path on one file system but those under `/srv/stage`, on another.
fn stage_apart(path: &Path) -> io::Result<u64> {
    Ok(if path.starts_with("/srv/stage") {
        ONE + 1
    } else {
        ONE
    })
}

/// A path that cannot be looked at: the cache directory.
fn cache_unreadable(path: &Path) -> io::Result<u64> {
    if path.starts_with("/var/lib/tierstore-cache") {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(ONE)
}

/// The run of the tests: the group and user `builders` (2001) and the numbers, on `volume`.
fn run(volume: fn(&Path) -> io::Result<u64>) -> PerRun {
    PerRun {
        host: Host {
            names: builders,
            volume,
        },
        ..DEFAULTS
    }
}

/// The tables of a build node without its interfaces; `server` goes in `[server]` and `local` in
/// `[local_cache]`.
fn build_node(server: &str, local: &str) -> String {
    format!(
        r#"
[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"
{server}

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"
{local}

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#
    )
}

/// The tables of a storage node with a cache, without its interfaces.
const COMBINED_NODE: &str = r#"
[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "key"

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[[policy]]
prefix = "cas/"
cache  = true
"#;

/// The tables of a storage node with no cache.
const STORAGE_NODE: &str = r#"
[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "key"

[hot_store]
kind   = "memory"
budget = 1000

[cold_store]
kind = "memory"
"#;

/// The tables of a node over the cold tier alone.
const COLD_NODE: &str = r#"
[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "key"

[cold_store]
kind = "memory"
"#;

/// The provider's socket of the examples.
const PROVIDER_SOCKET: &str = "/var/lib/tierstore/provider.sock";

/// The `s3` interface every file needs.
fn s3() -> String {
    "[[interface]]\ntype = \"s3\"\nparams = { port = 9300 }\n\n".to_owned()
}

/// A `mount-provider` interface with `more` after its socket.
fn provider(more: &str) -> String {
    format!(
        "[[interface]]\ntype = \"mount-provider\"\nparams = {{ socket = \"{PROVIDER_SOCKET}\"{more} }}\n\n"
    )
}

/// An `fs` interface whose params are the lines `params` (the needed two first), as the long form
/// of the design writes them.
fn fs(params: &str) -> String {
    format!(
        "[[interface]]\ntype = \"fs\"\n[interface.params]\nclient_group = \"builders\"\nowner = \"1000:1001\"\n{params}\n\n"
    )
}

/// The settings of `text` on `volume`, or its refusal.
fn load(text: &str, volume: fn(&Path) -> io::Result<u64>) -> Result<Option<FsSettings>, String> {
    let settings = settings_from_text(text, run(volume)).map_err(|why| why.to_string())?;
    Ok(settings.into_cache().expect("a build node").fs)
}

/// The settings of the build node with these entries and tables, which must load.
fn fs_of(entries: &[String], node: &str) -> FsSettings {
    let text = format!("{}{node}", entries.concat());
    load(&text, one_volume)
        .expect("a file that loads")
        .expect("an fs interface")
}

/// The refusal of the file with these entries and tables.
fn refusal(entries: &[String], node: &str) -> String {
    refusal_on(entries, node, one_volume)
}

/// The refusal of the file with these entries and tables, on `volume`.
fn refusal_on(entries: &[String], node: &str, volume: fn(&Path) -> io::Result<u64>) -> String {
    let text = format!("{}{node}", entries.concat());
    settings_from_text(&text, run(volume))
        .expect_err("refused")
        .to_string()
}

/// The `[fs]` table of `tierstore fs` that says what the params of `fs_params` say, read by its
/// own loader.
fn from_fs_table(text: &str) -> FsSettings {
    treefs::from_text(text, &builders).expect("an [fs] table that loads")
}

/// The keys that say the same thing in both tables: every param an `fs` interface takes, with a
/// value that is not its default.
const SAME: &str = r#"mountpoint = "/mnt/build"
runtime_dir = "/run/build-fs"
control_socket = "/run/build-fs/attach.sock"
fetch_timeout = "20s"
threads = 8
handoff_wait = "40s"
recover_wait = "50s"
reattach_concurrency = 4
tree_idle_secs = 60"#;

/// The `nfs` sub-table both say, every key but the hand-off ones, none at its default.
const SAME_NFS: &str = r#"listen = "127.0.0.1:12050"
mount = false
mount_user = "builder"
mount_wait = "5s"
mount_retry = "1s"
fd_cache_shards = 8
fd_cache_per_shard = 16
jukebox_after = "2s"
drain_wait = "3s"
dead_timeout = "90s"
unmount_wait = "10s""#;

/// The `put` sub-table both say, every key, none at its default.
const SAME_PUT: &str = r#"enabled = true
prefixes = ["cas/b/"]
stage_dir = "/var/lib/tierstore-cache/put-stage"
max_object_bytes = 10
max_dir_bytes = 20
max_dir_files = 30
min_free_bytes = 40
commit_deadline_secs = 50
stage_ttl_secs = 1000
committed_ttl_secs = 70"#;

/// Asserts that an `fs` interface makes the very settings the `[fs]` table of `tierstore fs` makes
/// of the same keys, with the node's own `local_cache.path` as the cache directory, its
/// `local_cache.fill_socket` as the fill socket, and `handoff.sock` in the runtime directory as the
/// hand-over socket of the mount; and that a file with only the needed params gets the `[fs]`
/// defaults. The put directory is in it on Linux, where it is served.
#[test]
fn an_fs_interface_makes_the_settings_the_fs_table_made() {
    let put = if cfg!(target_os = "linux") {
        format!("[fs.put]\n{SAME_PUT}\n")
    } else {
        String::new()
    };
    let table = format!(
        "[fs]\nclient_group = \"builders\"\nowner = \"1000:1001\"\nfill_socket = \"/var/lib/tierstore/fill.sock\"\ncache_path = \"/var/lib/tierstore-cache\"\n{SAME}\n[fs.nfs]\n{SAME_NFS}\n{put}"
    );
    let want = from_fs_table(&table);
    let put = put.replace("[fs.put]", "[interface.params.put]");
    let params = format!("{SAME}\n[interface.params.nfs]\n{SAME_NFS}\n{put}");
    let node = build_node("", "fill_socket = \"/var/lib/tierstore/fill.sock\"");
    let got = fs_of(&[s3(), fs(&params)], &node);
    assert_eq!(got, want);
    assert_eq!(got.cache_path, PathBuf::from("/var/lib/tierstore-cache"));
    assert_eq!(
        got.handoff_socket,
        PathBuf::from("/run/build-fs/handoff.sock")
    );
    assert_eq!(got.put.is_some(), cfg!(target_os = "linux"));

    let defaults = from_fs_table(
        "[fs]\nclient_group = \"builders\"\nowner = \"1000:1001\"\nfill_socket = \"/f.sock\"\ncache_path = \"/var/lib/tierstore-cache\"\n",
    );
    let got = fs_of(&[s3(), fs("")], &build_node("", ""));
    let want = FsSettings {
        fill_socket: None,
        ..defaults
    };
    assert_eq!(got, want);
}

/// Asserts that the node's fill socket in its state directory, where `local_cache.fill_socket` is
/// left out, is the file system's too, and that a file without an `fs` interface has no settings
/// of one.
#[test]
fn the_fill_socket_is_the_nodes_own_or_none() {
    let text = format!("{}{}{}", s3(), fs(""), build_node("", ""));
    let per_run = PerRun {
        state_dir: Some(PathBuf::from("/var/lib/tierstore")),
        ..run(one_volume)
    };
    let settings = settings_from_text(&text, per_run).expect("a build node");
    let fs = settings.into_cache().expect("a build node").fs;
    assert_eq!(
        fs.expect("an fs interface").fill_socket,
        Some(PathBuf::from("/var/lib/tierstore/fill.sock"))
    );
}

/// Asserts that a file of today, with no `fs` interface, loads as it did: a build node with a
/// provider at the default mount has no file system and keeps its provider's default mount and
/// its own given one, and a storage node, a storage node with a cache and a node over the cold
/// tier load as before.
#[test]
fn a_file_of_today_without_an_fs_interface_loads_as_today() {
    let text = format!("{}{}{}", s3(), provider(""), build_node("", ""));
    let settings = settings_from_text(&text, DEFAULTS).expect("a build node");
    let cache = settings.into_cache().expect("a build node");
    assert_eq!(cache.fs, None);
    let flags = cache.provider.expect("a provider");
    assert_eq!(flags.mountpoint, PathBuf::from("/run/tierstore/fs"));
    let own = provider(", mountpoint = \"/mnt/elsewhere\"");
    let text = format!("{}{own}{}", s3(), build_node("", ""));
    let settings = settings_from_text(&text, DEFAULTS).expect("a build node");
    let flags = settings.into_cache().expect("a build node").provider;
    assert_eq!(
        flags.expect("a provider").mountpoint,
        PathBuf::from("/mnt/elsewhere")
    );
    for node in [COMBINED_NODE, STORAGE_NODE, COLD_NODE] {
        let text = format!("{}{node}", s3());
        assert!(settings_from_text(&text, DEFAULTS).is_ok(), "{node}");
    }
}

/// Asserts that an `fs` interface is refused on every shape but a build node, with the words the
/// `trees` interface is refused with, naming what the node is.
#[test]
fn an_fs_interface_outside_a_build_node_is_refused() {
    let tail = "has no tree store: an fs interface needs a build node ([local_cache] and [cold_store] without [hot_store]), whose [trees] keep the trees";
    for (node, what) in [
        (STORAGE_NODE, "a storage node"),
        (COMBINED_NODE, "a storage node with a cache"),
        (COLD_NODE, "a node over the cold tier alone"),
    ] {
        assert_eq!(
            refusal(&[s3(), fs("")], node),
            format!("fs interface: {what} {tail}")
        );
    }
}

/// Asserts that a second `fs` interface is refused, naming its place, wherever the first is.
#[test]
fn a_second_fs_interface_is_refused() {
    let node = build_node("", "");
    assert_eq!(
        refusal(&[fs(""), s3(), fs("")], &node),
        "interface[2].type: a second fs interface: a node has one file system"
    );
}

/// Asserts that a param an `fs` interface does not have is refused, not warned of as `[fs]` warns:
/// at the top of the params, in `nfs` (a key `[fs.nfs]` has removed, too) and in `put`, each as a
/// fault of the file at its line, naming the key.
#[test]
fn an_fs_param_this_release_lacks_is_refused() {
    let node = build_node("", "");
    let top = refusal(&[s3(), fs("later_knob = 1")], &node);
    assert!(
        top.starts_with("line 10, column 1: unknown field `later_knob`, expected one of `port`"),
        "{top}"
    );
    let nfs = refusal(
        &[s3(), fs("[interface.params.nfs]\nallowed_uids = [501]")],
        &node,
    );
    assert!(
        nfs.contains("unknown field `allowed_uids`, expected one of `listen`, `mount`"),
        "{nfs}"
    );
    let put = refusal(
        &[
            s3(),
            fs("[interface.params.put]\nenabled = false\nlater = 1"),
        ],
        &node,
    );
    assert!(
        put.contains("unknown field `later`, expected one of `enabled`, `prefixes`"),
        "{put}"
    );
    let typed = refusal(
        &[
            s3(),
            fs("[interface.params.nfs]\nfd_cache_shards = \"many\""),
        ],
        &node,
    );
    assert!(
        typed.ends_with("not a value this key takes (it is not shown); expected u32"),
        "{typed}"
    );
    assert!(!typed.contains("many"), "{typed}");
}

/// Asserts that the three keys of `[fs]` the one file gives otherwise, and the hand-off keys of
/// `[fs.nfs]`, are refused as params by name, each with why, whatever their value.
#[test]
fn fill_socket_cache_path_and_handoff_socket_are_refused_as_fs_params() {
    let node = build_node("", "");
    let cases = [
        (
            "fill_socket = \"/var/lib/tierstore/fill.sock\"",
            "interface[1].params.fill_socket: not a param of an fs interface: the file system is served by the process that holds the cache, which it asks for fills itself",
        ),
        (
            "cache_path = 3",
            "interface[1].params.cache_path: not a param of an fs interface: the cache directory is local_cache.path of this file",
        ),
        (
            "handoff_socket = \"/run/tierstore-fs/handoff.sock\"",
            "interface[1].params.handoff_socket: not a param of an fs interface: the process hands over on server.handoff_socket",
        ),
    ];
    for (param, want) in cases {
        assert_eq!(refusal(&[s3(), fs(param)], &node), want);
    }
    let why =
        "not a param of an fs interface: the NFS front of the process has no hand-off of its own";
    for param in [
        "handoff_socket = \"/run/nfs.sock\"",
        "handoff_wait = \"5s\"",
        "handoff_ready_wait = \"5s\"",
    ] {
        let name = param.split(' ').next().expect("a name");
        let entry = fs(&format!("[interface.params.nfs]\n{param}"));
        assert_eq!(
            refusal(&[s3(), entry], &node),
            format!("interface[1].params.nfs.{name}: {why}")
        );
    }
}

/// Asserts that the `[fs]` refusals of a param name it under the interface's place, its range and
/// its defaults kept: a needed key left out, a path that is not absolute, a number out of range, a
/// name the system does not have, and a value of `nfs` and of `put` out of range.
#[test]
fn the_fs_refusals_name_the_param() {
    let node = build_node("", "");
    let without =
        |params: &str| format!("[[interface]]\ntype = \"fs\"\n[interface.params]\n{params}\n\n");
    let mut cases = vec![
        (
            without("owner = \"1:1\""),
            "interface[1].params.client_group: it is needed".to_owned(),
        ),
        (
            without("client_group = \"builders\""),
            "interface[1].params.owner: it is needed".to_owned(),
        ),
        (
            fs("mountpoint = \"fs\""),
            "interface[1].params.mountpoint: must be an absolute path".to_owned(),
        ),
        (
            fs("threads = 0"),
            "interface[1].params.threads: 0 is not between 1 and 252".to_owned(),
        ),
        (
            without("client_group = \"nobody-here\"\nowner = \"1:1\""),
            "interface[1].params.client_group: \"nobody-here\" is neither a group id nor the name of a group of this system".to_owned(),
        ),
        (
            without("client_group = \"1\"\nowner = \"1\""),
            "interface[1].params.owner: must be user:group, two parts joined by one colon, each a number or a name of this system, not \"1\"".to_owned(),
        ),
        (
            fs("fetch_timeout = \"60s\"\n[interface.params.nfs]\ndead_timeout = \"60s\""),
            "interface[1].params.nfs.dead_timeout: 60000ms is not more than interface[1].params.fetch_timeout (60000ms): a fill that the server still waits for would end the mount".to_owned(),
        ),
        (
            fs("[interface.params.nfs]\nlisten = \"192.0.2.1:2049\""),
            "interface[1].params.nfs.listen: must be an IPv4 loopback address: the protocol has no authentication a network could rely on".to_owned(),
        ),
        (
            fs("[interface.params.put]\nmax_object_bytes = 0"),
            "interface[1].params.put.max_object_bytes: must be at least 1".to_owned(),
        ),
    ];
    if cfg!(target_os = "linux") {
        cases.push((
            fs("[interface.params.put]\nenabled = true\nprefixes = [\"cas/b/\"]"),
            "interface[1].params.put.stage_dir: it is needed".to_owned(),
        ));
    }
    for (entry, want) in cases {
        assert_eq!(refusal(&[s3(), entry], &node), want);
    }
}

/// Asserts that a mount provider that leaves its `mountpoint` out answers under the `fs`
/// interface's mount, wherever either entry is, and under the default mount when the `fs`
/// interface leaves its own out too.
#[test]
fn a_provider_mountpoint_left_out_is_the_fs_mountpoint() {
    let node = build_node("", "");
    let mounted = fs("mountpoint = \"/private/var/tierstore/fs\"");
    for entries in [
        [s3(), provider(""), mounted.clone()],
        [mounted.clone(), s3(), provider("")],
    ] {
        let text = format!("{}{node}", entries.concat());
        let settings = settings_from_text(&text, run(one_volume)).expect("a build node");
        let flags = settings.into_cache().expect("a build node").provider;
        assert_eq!(
            flags.expect("a provider").mountpoint,
            PathBuf::from("/private/var/tierstore/fs")
        );
    }
    let text = format!("{}{}{}{node}", s3(), provider(""), fs(""));
    let settings = settings_from_text(&text, run(one_volume)).expect("a build node");
    let flags = settings.into_cache().expect("a build node").provider;
    assert_eq!(
        flags.expect("a provider").mountpoint,
        PathBuf::from("/run/tierstore/fs")
    );
}

/// Asserts that a mount provider that gives a `mountpoint` other than the `fs` interface's is
/// refused, naming the provider's param and both mounts; and that one that gives the same mount
/// loads.
#[test]
fn a_provider_mountpoint_other_than_the_fs_one_is_refused() {
    let node = build_node("", "");
    let elsewhere = provider(", mountpoint = \"/mnt/elsewhere\"");
    assert_eq!(
        refusal(&[s3(), elsewhere, fs("")], &node),
        "interface[1].params.mountpoint: \"/mnt/elsewhere\" is not the mountpoint of the fs interface, interface[2].params.mountpoint (\"/run/tierstore/fs\"): the provider answers a source's directory under that mount; leave it out to take it"
    );
    let same = provider(", mountpoint = \"/mnt/build\"");
    let text = format!("{}{same}{}{node}", s3(), fs("mountpoint = \"/mnt/build\""));
    assert!(settings_from_text(&text, run(one_volume)).is_ok());
}

/// Asserts that a stage directory of the put directory on another file system than
/// `local_cache.path` is refused, naming the param; that one on the same file system loads; and
/// that a path that cannot be looked at is refused naming its key and what the system said.
#[cfg(target_os = "linux")]
#[test]
fn a_put_stage_off_the_cache_volume_is_refused() {
    let node = build_node("", "");
    let put = |stage: &str| {
        fs(&format!(
            "[interface.params.put]\nenabled = true\nprefixes = [\"cas/b/\"]\nstage_dir = \"{stage}\""
        ))
    };
    assert_eq!(
        refusal_on(&[s3(), put("/srv/stage/put")], &node, stage_apart),
        "interface[1].params.put.stage_dir: \"/srv/stage/put\" is not on the file system of local_cache.path: a staged file becomes a file of the cache by a rename"
    );
    let text = format!("{}{}{node}", s3(), put("/srv/put"));
    let fs = load(&text, stage_apart)
        .expect("a file that loads")
        .expect("an fs interface");
    assert_eq!(
        fs.put.expect("a put directory").stage_dir,
        PathBuf::from("/srv/put")
    );
    assert_eq!(
        refusal_on(&[s3(), put("/srv/stage/put")], &node, cache_unreadable),
        "local_cache.path: \"/var/lib/tierstore-cache\" could not be looked at: permission denied"
    );
    let unreadable_stage = put("/var/lib/tierstore-cache/put");
    assert_eq!(
        refusal_on(&[s3(), unreadable_stage], &node, cache_unreadable),
        "interface[1].params.put.stage_dir: \"/var/lib/tierstore-cache/put\" could not be looked at: permission denied"
    );
}

/// Asserts that a runtime directory inside the cache directory is refused, also when it is written
/// with `..`, and that one beside it loads.
#[test]
fn a_runtime_dir_inside_the_local_cache_is_refused() {
    let node = build_node("", "");
    for dir in [
        "/var/lib/tierstore-cache/fs",
        "/var/lib/other/../tierstore-cache/fs",
    ] {
        assert_eq!(
            refusal(&[s3(), fs(&format!("runtime_dir = \"{dir}\""))], &node),
            format!(
                "interface[1].params.runtime_dir: \"{dir}\" is inside local_cache.path: it is not a place for the cache's files"
            )
        );
    }
    let beside = fs("runtime_dir = \"/var/lib/tierstore-cache-fs\"");
    let fs = fs_of(&[s3(), beside], &node);
    assert_eq!(fs.runtime_dir, PathBuf::from("/var/lib/tierstore-cache-fs"));
}

/// Asserts that a control socket that is `server.handoff_socket` is refused, and so is one that is
/// the fill socket or the mount provider's socket, each naming the other socket.
#[test]
fn a_control_socket_equal_to_the_handoff_socket_is_refused() {
    let socket = "/var/lib/tierstore/node.sock";
    let control = fs(&format!("control_socket = \"{socket}\""));
    let tail = "the two sockets are two files";
    let handoff = build_node(&format!("handoff_socket = \"{socket}\""), "");
    assert_eq!(
        refusal(&[s3(), control.clone()], &handoff),
        format!(
            "interface[1].params.control_socket: \"{socket}\" is server.handoff_socket too; {tail}"
        )
    );
    let fill = build_node("", &format!("fill_socket = \"{socket}\""));
    assert_eq!(
        refusal(&[s3(), control.clone()], &fill),
        format!(
            "interface[1].params.control_socket: \"{socket}\" is local_cache.fill_socket too; {tail}"
        )
    );
    let shared = fs(&format!("control_socket = \"{PROVIDER_SOCKET}\""));
    assert_eq!(
        refusal(&[s3(), provider(""), shared], &build_node("", "")),
        format!(
            "interface[2].params.control_socket: \"{PROVIDER_SOCKET}\" is mount-provider interface params.socket too; {tail}"
        )
    );
}

/// Asserts that a build node with an `fs` interface needs an absolute `local_cache.path`, which
/// the file system reads the cache's files by.
#[test]
fn a_relative_cache_path_is_refused_with_an_fs_interface() {
    let node = build_node("", "").replace(
        "path   = \"/var/lib/tierstore-cache\"",
        "path   = \"cache\"",
    );
    assert_eq!(
        refusal(&[s3(), fs("")], &node),
        "local_cache.path: must be an absolute path in a file with an fs interface: the file system reads the cache's files by it"
    );
}

/// Asserts that an `fs` interface takes no param of the other types, and the other types take no
/// param only an `fs` interface has.
#[test]
fn the_fs_params_and_the_other_types_params_are_apart() {
    let node = build_node("", "");
    for param in [
        "port = 9700",
        "addresses = [\"127.0.0.1:9700\"]",
        "tls_cert_file = \"/a\"",
        "tls_key_file = \"/k\"",
        "prefix = \"oci/\"",
        "max_connections = 1",
        "connection_lifetime = \"1m\"",
        "require_auth = true",
        "stage_read = 1",
        "max_manifest_bytes = 1",
        "read_chunk_bytes = 1",
        "echo_bytes = 1",
        "upstream = { url = \"https://registry.example\" }",
        "socket = \"/s.sock\"",
    ] {
        let name = param.split(' ').next().expect("a name");
        assert_eq!(
            refusal(&[s3(), fs(param)], &node),
            format!(
                "interface[1].params.{name}: not for type \"fs\": the file system is served on its mount point alone"
            )
        );
    }
    let theirs = [
        "owner = \"1:1\"",
        "client_group = \"1\"",
        "runtime_dir = \"/r\"",
        "control_socket = \"/c.sock\"",
        "threads = 1",
        "handoff_wait = \"5s\"",
        "recover_wait = \"5s\"",
        "reattach_concurrency = 1",
        "tree_idle_secs = 1",
        "nfs = {}",
        "put = {}",
        "fill_socket = \"/f.sock\"",
        "cache_path = \"/c\"",
        "handoff_socket = \"/h.sock\"",
    ];
    for (kind, listen) in [
        ("s3", "port = 9301, "),
        ("trees", "port = 9600, "),
        ("oci", "addresses = [\"127.0.0.1:5000\"], "),
        ("mount-provider", "socket = \"/p.sock\", "),
    ] {
        for param in theirs {
            let name = param.split(' ').next().expect("a name");
            let entry =
                format!("[[interface]]\ntype = \"{kind}\"\nparams = {{ {listen}{param} }}\n\n");
            assert_eq!(
                refusal(&[s3(), entry], &node),
                format!("interface[1].params.{name}: not for type \"{kind}\"")
            );
        }
    }
}

/// Asserts the line a start says of an `fs` interface it does not serve, and none without one.
#[test]
fn a_start_says_it_does_not_serve_the_fs_interface() {
    assert_eq!(not_served(None), None);
    let fs = fs_of(&[s3(), fs("")], &build_node("", ""));
    assert_eq!(
        not_served(Some(&fs)).as_deref(),
        Some(
            "tierstore: the fs interface (mountpoint /run/tierstore/fs) is checked and not served by this release; `tierstore fs`, with its own [fs] table, serves the trees"
        )
    );
}

/// Asserts that the file system of a path is that of the path when it is there, of the nearest
/// directory above it that is there when it is not, and that a path whose part is a file, or a
/// relative path with no part there, is an error.
#[test]
fn the_volume_of_a_path_is_that_of_its_nearest_directory_that_is_there() {
    let dir = TestDir::under(std::env::temp_dir(), "tsconfig-fs-volume");
    let there = volume_of(dir.path()).expect("the scratch directory");
    let deeper = dir.path().join("not").join("made");
    assert_eq!(volume_of(&deeper).expect("its directory"), there);
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").expect("a file");
    let under = volume_of(&file.join("child")).expect_err("not a directory");
    assert_eq!(under.kind(), io::ErrorKind::NotADirectory);
    let nowhere = volume_of(Path::new("not-here-at-all")).expect_err("nothing there");
    assert_eq!(nowhere.kind(), io::ErrorKind::NotFound);
    assert_eq!(nowhere.to_string(), "no part of the path is there");
}

/// Asserts that the file systems a run looks paths up on are the system's unless a test gives its
/// own.
#[test]
fn the_host_of_a_run_is_the_system() {
    let dir = TestDir::under(std::env::temp_dir(), "tsconfig-fs-host");
    let found = (Host::default().volume)(dir.path()).expect("the scratch directory");
    assert_eq!(found, volume_of(dir.path()).expect("the scratch directory"));
    assert_eq!(
        (PerRun::default().host.volume)(dir.path()).ok(),
        Some(found)
    );
}
