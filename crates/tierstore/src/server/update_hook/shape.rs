//! What the hook reads of a node's file about its `fs` interface (`docs/design/one-service.md`
//! sections 1.2 and 3.6): whether the node holds a mount, and the three paths a successor must
//! share with the instance that runs to take the mount over (the mount point, the control socket
//! and the runtime directory, which holds the hand-over socket of the mount and its state); and
//! about its `mount-provider` interface: the socket `verify` asks and the mount its trees are under.
//!
//! The file is read as TOML, not loaded: the hook does not resolve users or groups, nor look at
//! the disk. A path a param leaves out is the default the loader gives it (`treefs/config.rs`).
//! What the loader would refuse is the release's `--check-config` to refuse; here an `fs` entry
//! whose path params are not strings is taken as it would read for the comparison: its own value,
//! so two files that differ there differ.

use std::path::PathBuf;

use crate::server::provider::DEFAULT_MOUNTPOINT as PROVIDER_DEFAULT_MOUNTPOINT;
use crate::server::treefs::{DEFAULT_CONTROL_SOCKET, DEFAULT_MOUNTPOINT, DEFAULT_RUNTIME_DIR};

/// The `type` of the interface that serves the trees as a file system.
pub(super) const FS_INTERFACE: &str = "fs";
/// The `type` of the interface that answers a job agent's `Prepare` and `Mount` with a directory
/// under the mount.
pub(super) const PROVIDER_INTERFACE: &str = "mount-provider";

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

/// The first interface of type `kind` of the file whose text is `text`: `None` for a file with
/// none, and for a text that is not TOML.
fn interface(text: &str, kind: &str) -> Option<toml::Table> {
    let table: toml::Table = text.parse().ok()?;
    table
        .get("interface")?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_table)
        .find(|entry| entry.get("type").and_then(toml::Value::as_str) == Some(kind))
        .cloned()
}

/// Whether the file whose text is `text` has a `mount-provider` interface. A text that is not
/// TOML has none.
pub(super) fn has_provider(text: &str) -> bool {
    interface(text, PROVIDER_INTERFACE).is_some()
}

/// The `fs` interface of the file whose text is `text`: `None` for a file with none, and for a
/// text that is not TOML (the release's check of the file refuses that).
pub(super) fn fs_interface(text: &str) -> Option<FsShape> {
    let entry = interface(text, FS_INTERFACE)?;
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

/// Where a file's `mount-provider` interface is served: its socket, and the mount of
/// `tierstore fs` its trees are placed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct ProviderShape {
    /// `params.socket`.
    pub(in crate::server) socket: PathBuf,
    /// `params.mountpoint`, or the loader's default.
    pub(in crate::server) mountpoint: PathBuf,
}

/// The `mount-provider` interface of the file whose text is `text`: `None` for a file with none
/// (or a text that is not TOML); an error for an entry whose socket or mount point is not an
/// absolute path, which the release's check of the file refuses.
pub(super) fn provider_interface(text: &str) -> Result<Option<ProviderShape>, String> {
    let Some(entry) = interface(text, PROVIDER_INTERFACE) else {
        return Ok(None);
    };
    let params = entry.get("params").and_then(toml::Value::as_table);
    let path = |key: &str, default: Option<&str>| -> Result<PathBuf, String> {
        let given = params.and_then(|params| params.get(key));
        let text = match given {
            Some(value) => value.as_str(),
            None => default,
        };
        text.map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| format!("the {PROVIDER_INTERFACE} interface has no absolute {key}"))
    };
    Ok(Some(ProviderShape {
        socket: path("socket", None)?,
        mountpoint: path("mountpoint", Some(PROVIDER_DEFAULT_MOUNTPOINT))?,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{FsShape, ProviderShape, fs_interface, has_provider, provider_interface};

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

    /// Asserts that a file has a mount provider when one of its interfaces has the type
    /// `mount-provider`, beside others or alone, and none when no interface has that type, when
    /// `interface` is no array, or when the text is not TOML. Catches a Mac of the two-process
    /// layout taken for one with nothing to serve, whose update would then leave its provider
    /// answering directories no process serves.
    #[test]
    fn a_mount_provider_is_found_among_the_interfaces() {
        let provider = "[[interface]]\ntype = \"mount-provider\"\n";
        let s3 = "[[interface]]\ntype = \"s3\"\n";
        assert!(has_provider(provider));
        assert!(has_provider(&format!("{s3}{provider}")));
        for none in [
            "",
            s3,
            "[[interface]]\ntype = \"fs\"\n",
            "interface = 1\n",
            "[[interface]]\nparams = {}\n",
            "= not toml",
        ] {
            assert!(!has_provider(none), "{none:?}");
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

    /// Asserts that a file's `mount-provider` interface is found among its interfaces with its
    /// socket and its mount point, the mount point the loader's default when it is left out; that
    /// a file with none, or a text that is not TOML, has none; and that an entry whose socket is
    /// missing, not a string or not absolute, or whose mount point is not absolute, is refused by
    /// name. Catches a build node whose check is skipped because its entry was missed, and a socket
    /// taken from an entry the release refuses.
    #[test]
    fn the_provider_interface_is_read_with_the_loaders_default_mount() {
        let entry = |params: &str| {
            format!(
                "[[interface]]\ntype = \"s3\"\n[[interface]]\ntype = \"mount-provider\"\n{params}"
            )
        };
        let shape = |socket: &str, mountpoint: &str| ProviderShape {
            socket: PathBuf::from(socket),
            mountpoint: PathBuf::from(mountpoint),
        };
        assert_eq!(
            provider_interface(&entry("params = { socket = \"/var/p.sock\" }\n")),
            Ok(Some(shape("/var/p.sock", "/run/tierstore/fs")))
        );
        assert_eq!(
            provider_interface(&entry(
                "params = { socket = \"/var/p.sock\", mountpoint = \"/mnt/fs\" }\n"
            )),
            Ok(Some(shape("/var/p.sock", "/mnt/fs")))
        );
        for none in [
            "",
            "[server]\n",
            "[[interface]]\ntype = \"fs\"\n",
            "interface = 1\n",
            "= x",
        ] {
            assert_eq!(provider_interface(none), Ok(None), "{none:?}");
        }
        let no_socket = "the mount-provider interface has no absolute socket".to_owned();
        for params in [
            "",
            "params = {}\n",
            "params = { socket = 5 }\n",
            "params = { socket = \"p.sock\" }\n",
        ] {
            assert_eq!(
                provider_interface(&entry(params)),
                Err(no_socket.clone()),
                "{params:?}"
            );
        }
        assert_eq!(
            provider_interface(&entry(
                "params = { socket = \"/p.sock\", mountpoint = \"fs\" }\n"
            )),
            Err("the mount-provider interface has no absolute mountpoint".to_owned())
        );
    }
}
