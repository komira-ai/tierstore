//! The node file the rig starts the node with: the farm's shape, a build node (a local cache over
//! an S3 upstream) that serves the mount provider and, but in the case of a node without it, the
//! `fs` mount, with the rig's own paths, its
//! own user and group, and none of the farm's names, addresses or keys. The upstream is
//! `fake-s3`, by its loopback address; the S3 front listens on a free loopback port, which the
//! node's start line names.
//!
//! Every file of a node is in its case's directory: the node file, its two key files (mode 0600,
//! the rig's user's, as tierstore asks), the provider's socket, the mount point and the mount's
//! runtime directory. The cache may be elsewhere (a small disk of its own).

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// The most bytes of a Unix socket's path that every system tierstore runs on binds.
pub(crate) const MOST_SOCKET_PATH: usize = 103;

/// The access key the node's S3 front takes, and the key it signs its upstream requests with:
/// loopback keys of a test, which `fake-s3` does not check.
const ACCESS_KEY: &str = "blackbox\nblackbox-front-secret-of-the-test\n";

/// The id and the secret of [`ACCESS_KEY`], for a client of the node's S3 front.
pub(crate) const FRONT_KEY: (&str, &str) = ("blackbox", "blackbox-front-secret-of-the-test");
const UPSTREAM_KEY: &str = "blackbox-upstream\nblackbox-upstream-secret-of-the-test\n";

/// The files of a node in its case's directory.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub dir: PathBuf,
    pub file: PathBuf,
    pub socket: PathBuf,
    pub mountpoint: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    /// The files of a node whose case's directory is `dir`.
    pub(crate) fn of(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            file: dir.join("node.toml"),
            socket: dir.join("provider.sock"),
            mountpoint: dir.join("fs"),
            logs: dir.join("logs"),
        }
    }
}

/// What a node file says beyond its paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Node {
    /// The local cache's directory.
    pub cache: PathBuf,
    /// The cache's budget, in MiB.
    pub budget_mib: u64,
    /// The upstream's loopback address, `127.0.0.1:<port>`.
    pub upstream: String,
    /// `trees.max_placements`, when the case sets it; the node's default otherwise.
    pub max_placements: Option<u32>,
    /// Whether the node has its `fs` interface: the FUSE mount, which needs `/dev/fuse` and root.
    pub fs: bool,
    /// The user and group the mount is owned by and served to: the rig's own.
    pub uid: u32,
    pub gid: u32,
}

impl Node {
    /// The `fs` interface of the node file at `paths`, with the blank line after it; nothing for a
    /// node without one.
    fn fs_interface(&self, paths: &Paths) -> String {
        if !self.fs {
            return String::new();
        }
        format!(
            r#"
[[interface]]
type = "fs"
[interface.params]
mountpoint   = "{mountpoint}"
runtime_dir  = "{dir}/fsrun"
client_group = "{gid}"
owner        = "{uid}:{gid}"
"#,
            mountpoint = paths.mountpoint.display(),
            dir = paths.dir.display(),
            uid = self.uid,
            gid = self.gid,
        )
    }

    /// The text of the node file at `paths`.
    pub(crate) fn text(&self, paths: &Paths) -> String {
        let dir = paths.dir.display();
        let mut text = format!(
            r#"# The node file of tierstore's black-box test: a build node of the farm's shape.
[[interface]]
type   = "s3"
params = {{ addresses = ["127.0.0.1:0"] }}

[[interface]]
type   = "mount-provider"
params = {{ socket = "{socket}" }}
{fs}
[server]
proxy_id        = "blackbox"
bucket          = "cas"
access_key_file = "{dir}/access.key"
drain_lb_wait   = "1s"
drain_deadline  = "10s"

[local_cache]
path   = "{cache}"
budget = "{budget}MiB"

[cold_store]
kind             = "s3"
bucket           = "cas"
endpoint         = "http://{upstream}"
credentials_file = "{dir}/upstream.key"

[[policy]]
prefix = "cas/"
cache  = true
"#,
            socket = paths.socket.display(),
            fs = self.fs_interface(paths),
            cache = self.cache.display(),
            budget = self.budget_mib,
            upstream = self.upstream,
        );
        if let Some(most) = self.max_placements {
            text.push_str(&format!("\n[trees]\nmax_placements = {most}\n"));
        }
        text
    }

    /// Writes the node file and its key files at `paths`, making the case's directory and its
    /// log directory; the node file's path.
    ///
    /// # Errors
    /// A provider's socket whose path is too long to bind, or a file that cannot be written.
    pub(crate) fn write(&self, paths: &Paths) -> Result<PathBuf, String> {
        let long = paths.socket.as_os_str().len();
        if long > MOST_SOCKET_PATH {
            return Err(format!(
                "the provider's socket {} is {long} bytes, over {MOST_SOCKET_PATH}: give a shorter scratch directory",
                paths.socket.display()
            ));
        }
        std::fs::create_dir_all(&paths.logs)
            .map_err(|why| format!("{}: {why}", paths.logs.display()))?;
        secret(&paths.dir.join("access.key"), ACCESS_KEY)?;
        secret(&paths.dir.join("upstream.key"), UPSTREAM_KEY)?;
        std::fs::write(&paths.file, self.text(paths))
            .map_err(|why| format!("{}: {why}", paths.file.display()))?;
        Ok(paths.file.clone())
    }
}

/// Writes `text` to a new file at `path` that only its owner reads.
fn secret(path: &Path, text: &str) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|why| format!("{}: {why}", path.display()))
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
