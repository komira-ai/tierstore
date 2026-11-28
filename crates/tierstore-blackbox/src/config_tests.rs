//! The node file of the rig, and the files written beside it.

use std::os::unix::fs::PermissionsExt as _;

use tierstore_testkit::dir::TestDir;

use super::*;

fn node(cache: &Path) -> Node {
    Node {
        cache: cache.to_owned(),
        budget_mib: 256,
        upstream: "127.0.0.1:4567".to_owned(),
        max_placements: None,
        uid: 1000,
        gid: 1001,
    }
}

/// Asserts the whole node file of a case: the farm's interfaces at the case's paths, the rig's
/// user and group, the upstream by its address, the budget in MiB, and no `[trees]` table when
/// the case leaves `max_placements` to the node.
#[test]
fn the_node_file_is_the_farms_shape_at_the_cases_paths() {
    let paths = Paths::of(Path::new("/s/t1"));
    let text = node(Path::new("/s/t1/cache")).text(&paths);
    assert_eq!(
        text,
        r#"# The node file of tierstore's black-box test: a build node of the farm's shape.
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[[interface]]
type   = "mount-provider"
params = { socket = "/s/t1/provider.sock" }

[[interface]]
type = "fs"
[interface.params]
mountpoint   = "/s/t1/fs"
runtime_dir  = "/s/t1/fsrun"
client_group = "1001"
owner        = "1000:1001"

[server]
proxy_id        = "blackbox"
bucket          = "cas"
access_key_file = "/s/t1/access.key"
drain_lb_wait   = "1s"
drain_deadline  = "10s"

[local_cache]
path   = "/s/t1/cache"
budget = "256MiB"

[cold_store]
kind             = "s3"
bucket           = "cas"
endpoint         = "http://127.0.0.1:4567"
credentials_file = "/s/t1/upstream.key"

[[policy]]
prefix = "cas/"
cache  = true
"#
    );
    assert_eq!(paths.mountpoint, Path::new("/s/t1/fs"));
    assert_eq!(paths.logs, Path::new("/s/t1/logs"));
}

/// Asserts that a case that sets `max_placements` gets a `[trees]` table with it, last.
#[test]
fn a_case_that_sets_max_placements_gets_a_trees_table() {
    let paths = Paths::of(Path::new("/s/t2"));
    let mut node = node(Path::new("/s/t2/cache"));
    node.max_placements = Some(64);
    assert!(
        node.text(&paths)
            .ends_with("cache  = true\n\n[trees]\nmax_placements = 64\n")
    );
}

/// Asserts that writing a node makes its directory and log directory, writes the node file with
/// its text, and writes both key files readable by their owner alone.
#[test]
fn writing_a_node_writes_its_file_and_owner_only_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "blackbox-config");
    let paths = Paths::of(&dir.path().join("case"));
    let node = node(&dir.path().join("cache"));
    let file = node.write(&paths).unwrap();
    assert_eq!(file, paths.file);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), node.text(&paths));
    assert!(paths.logs.is_dir());
    for (key, text) in [("access.key", ACCESS_KEY), ("upstream.key", UPSTREAM_KEY)] {
        let path = paths.dir.join(key);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{key}");
    }
    // A second write finds the keys there and says so, naming the file.
    let again = node.write(&paths).unwrap_err();
    assert!(
        again.starts_with(&format!("{}: ", paths.dir.join("access.key").display())),
        "{again}"
    );
}

/// Asserts that a case's directory too deep for the provider's socket is refused before
/// anything is written, naming the socket and the limit; and that a log directory that cannot be
/// made is named.
#[test]
fn a_node_that_cannot_be_written_says_why() {
    let deep = format!("/{}", "d".repeat(100));
    let paths = Paths::of(Path::new(&deep));
    let refused = node(Path::new("/c")).write(&paths).unwrap_err();
    assert_eq!(
        refused,
        format!(
            "the provider's socket {deep}/provider.sock is 115 bytes, over 103: give a shorter scratch directory"
        )
    );
    assert!(!Path::new(&deep).exists());

    let dir = TestDir::under(std::env::temp_dir(), "blackbox-config-file");
    std::fs::write(dir.path().join("case"), b"a file").unwrap();
    let paths = Paths::of(&dir.path().join("case"));
    let refused = node(Path::new("/c")).write(&paths).unwrap_err();
    assert!(
        refused.starts_with(&format!("{}: ", paths.logs.display())),
        "{refused}"
    );
}
