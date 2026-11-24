//! What the hook reads of a node's file about its `fs` interface (`docs/design/one-service.md`
//! sections 1.2 and 3.6): whether the node holds a mount, and the three paths a successor must
//! share with the instance that runs to take the mount over (the mount point, the control socket
//! and the runtime directory, which holds the hand-over socket of the mount and its state).
//!
//! The file is read as TOML, not loaded: the hook does not resolve users or groups, nor look at
//! the disk. A path a param leaves out is the default the loader gives it (`treefs/config.rs`).
//! What the loader would refuse is the release's `--check-config` to refuse; here an `fs` entry
//! whose path params are not strings is taken as it would read for the comparison: its own value,
//! so two files that differ there differ.

use std::path::PathBuf;

use crate::server::treefs::{DEFAULT_CONTROL_SOCKET, DEFAULT_MOUNTPOINT, DEFAULT_RUNTIME_DIR};

/// The `type` of the interface that serves the trees as a file system.
pub(super) const FS_INTERFACE: &str = "fs";

/// The paths of a file's `fs` interface that a successor shares with the instance it takes the
/// mount from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FsShape {
    /// `params.mountpoint`.
    pub(super) mountpoint: PathBuf,
    /// `params.control_socket`.
    pub(super) control_socket: PathBuf,
    /// `params.runtime_dir`.
    pub(super) runtime_dir: PathBuf,
}

impl FsShape {
    /// The names of the paths that differ between `self` and `other`, in the order of the params.
    pub(super) fn moved(&self, other: &FsShape) -> Vec<&'static str> {
        let mut moved = Vec::new();
        if self.mountpoint != other.mountpoint {
            moved.push("mountpoint");
        }
        if self.control_socket != other.control_socket {
            moved.push("control_socket");
        }
        if self.runtime_dir != other.runtime_dir {
            moved.push("runtime_dir");
        }
        moved
    }
}

/// The `fs` interface of the file whose text is `text`: `None` for a file with none, and for a
/// text that is not TOML (the release's check of the file refuses that).
pub(super) fn fs_interface(text: &str) -> Option<FsShape> {
    let table: toml::Table = text.parse().ok()?;
    let entry = table
        .get("interface")?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_table)
        .find(|entry| entry.get("type").and_then(toml::Value::as_str) == Some(FS_INTERFACE))?;
    let params = entry.get("params").and_then(toml::Value::as_table);
    let path = |key: &str| -> Option<PathBuf> {
        params.and_then(|params| params.get(key)).map(|value| {
            PathBuf::from(
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned),
            )
        })
    };
    let runtime_dir = path("runtime_dir").unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR));
    Some(FsShape {
        mountpoint: path("mountpoint").unwrap_or_else(|| PathBuf::from(DEFAULT_MOUNTPOINT)),
        control_socket: path("control_socket")
            .unwrap_or_else(|| runtime_dir.join(DEFAULT_CONTROL_SOCKET)),
        runtime_dir,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FsShape, fs_interface};

    fn shape(mountpoint: &str, control_socket: &str, runtime_dir: &str) -> FsShape {
        FsShape {
            mountpoint: PathBuf::from(mountpoint),
            control_socket: PathBuf::from(control_socket),
            runtime_dir: PathBuf::from(runtime_dir),
        }
    }

    /// Asserts that a file's `fs` interface is found among its interfaces, with the paths its
    /// params give and the loader's defaults for those it leaves out (the control socket in the
    /// runtime directory, wherever that is); and that a file with no `fs` interface, with
    /// interfaces of other types, with an `interface` that is no array, or that is not TOML, has
    /// none. Catches a node with a mount taken for one without (whose update would then restart
    /// and drop the mount), and a default that differs from the loader's.
    #[test]
    fn the_fs_interface_is_read_with_the_loaders_defaults() {
        let s3 = "[[interface]]\ntype = \"s3\"\n";
        let fs = "[[interface]]\ntype = \"fs\"\n";
        assert_eq!(
            fs_interface(&format!("{s3}{fs}")),
            Some(shape(
                "/run/tierstore/fs",
                "/run/tierstore-fs/control.sock",
                "/run/tierstore-fs"
            ))
        );
        let given = format!(
            "{fs}[interface.params]\nmountpoint = \"/mnt/t\"\ncontrol_socket = \"/run/c.sock\"\nruntime_dir = \"/run/x\"\n"
        );
        assert_eq!(
            fs_interface(&given),
            Some(shape("/mnt/t", "/run/c.sock", "/run/x"))
        );
        let runtime = format!("{fs}[interface.params]\nruntime_dir = \"/run/x\"\n");
        assert_eq!(
            fs_interface(&runtime),
            Some(shape("/run/tierstore/fs", "/run/x/control.sock", "/run/x"))
        );
        // A path that is not a string is kept as it reads, so that a change of it is seen.
        let odd = format!("{fs}[interface.params]\nmountpoint = 5\n");
        assert_eq!(
            fs_interface(&odd).map(|shape| shape.mountpoint),
            Some(PathBuf::from("5"))
        );
        for none in [
            "",
            "[server]\n",
            s3,
            "interface = 1\n",
            "[[interface]]\nparams = {}\n",
            "interface = [1]\n",
            "= not toml",
        ] {
            assert_eq!(fs_interface(none), None, "{none:?}");
        }
    }

    /// Asserts that `moved` names each path that differs, in the order of the params, and none
    /// for equal shapes. Catches a move of the control socket or the runtime directory that a
    /// config update would take for a hand-over, which the new instance then refuses with a
    /// `nak`.
    #[test]
    fn moved_names_every_path_that_differs() {
        let a = shape("/m", "/c", "/r");
        assert!(a.moved(&a).is_empty());
        assert_eq!(a.moved(&shape("/n", "/c", "/r")), ["mountpoint"]);
        assert_eq!(a.moved(&shape("/m", "/d", "/r")), ["control_socket"]);
        assert_eq!(a.moved(&shape("/m", "/c", "/s")), ["runtime_dir"]);
        assert_eq!(
            a.moved(&shape("/n", "/d", "/s")),
            ["mountpoint", "control_socket", "runtime_dir"]
        );
    }
}
