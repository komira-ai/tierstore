//! The volume of a Mac in an update of the release: the mount of the node's `fs` interface, under
//! which the mount provider answers a job's directories. An update is not verified unless that
//! volume is served, and an update of a node whose file would leave it served by no process is
//! refused before anything is stopped.
//!
//! - `prepare` refuses (`mode_unsupported`) a node whose file has a `mount-provider` interface and
//!   no `fs` interface: the two-process layout, whose mount was served by `tierstore fs` as a job
//!   of its own. No release since 0.3.196 runs that process (`tierstore fs --config` exits 2), and
//!   this hook starts no fs job, so after the update nothing would serve the provider's mount. The
//!   node is switched to one process first, by its config package (`config_hook/switch.rs`); the
//!   refusal records and stops nothing, so the node stays on the release it runs.
//! - `verify`, before the commit, of a node whose file has an `fs` interface requires the mount
//!   listed at that interface's mount point, and a look-up of `trees` in it to find a directory:
//!   the namespace every mount of tierstore's file system has at its root, which a directory with
//!   nothing mounted on it lacks, and which the server must answer. The mount point is the
//!   provider's root too (the loader holds the provider's `mountpoint` to the fs interface's). A
//!   mount that is not listed, or whose look-up finds nothing or fails, fails `verify` with
//!   `not_ready`, and hostkeeper rolls the update back. A node whose file has no `fs` interface
//!   serves no volume, and there is nothing to prove.
//!
//! Both are the update hook's alone (`releases`): the config hook drives the arm for its own
//! packages, and its switch is the way off the two-process layout.

use std::path::Path;

use super::super::params::Params;
use super::super::report::{Done, Reason};
use super::super::shape::{fs_interface, has_provider};
use super::Ctx;

/// The directory at the root of every mount of tierstore's file system.
const NAMESPACE_DIR: &str = "trees";

impl Ctx<'_, '_> {
    /// Refuses, in `prepare` of the release, a node of the two-process layout: its file (`text`,
    /// at `config`) has a mount provider and no `fs` interface.
    pub(super) fn one_process(&self, config: &Path, text: &str) -> Result<(), Done> {
        if !self.releases || fs_interface(text).is_some() || !has_provider(text) {
            return Ok(());
        }
        Err(Done::fail(
            Reason::ModeUnsupported,
            format!(
                "{} has a mount-provider interface and no fs interface: the two-process layout, \
                 whose mount tierstore fs served as a job of its own. No release since 0.3.196 \
                 runs that process, so after this update nothing would serve the provider's \
                 mount. Switch the node to one process first (an [[interface]] with type = \"fs\" \
                 in its file, through its config package); nothing was stopped",
                config.display()
            ),
        ))
    }

    /// `verify`'s proof of the volume, in an update of the release: when the node's file has an
    /// `fs` interface, its mount point is listed as a mount and `trees` in it is a directory.
    pub(super) fn volume_served(&self, params: &Params) -> Result<(), Done> {
        let Some(config) = params.config_file.as_deref().filter(|_| self.releases) else {
            return Ok(());
        };
        let Some(text) = self.ports.host.config(config)? else {
            return Err(Done::fail(
                Reason::ConfigMissing,
                format!(
                    "there is no configuration file at {}: the volume to look for is not known",
                    config.display()
                ),
            ));
        };
        let Some(shape) = fs_interface(&text) else {
            return Ok(());
        };
        let mountpoint = &shape.mountpoint;
        if !self.ports.host.mounted(mountpoint)? {
            return Err(Done::fail(
                Reason::NotReady,
                format!(
                    "{} is not mounted: nothing serves the volume of the fs interface, under \
                     which the mount provider answers",
                    mountpoint.display()
                ),
            ));
        }
        let namespace = mountpoint.join(NAMESPACE_DIR);
        match self.ports.host.is_dir(&namespace) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Done::fail(
                Reason::NotReady,
                format!(
                    "{} is mounted and {} is not a directory: the mount is not tierstore's file \
                     system",
                    mountpoint.display(),
                    namespace.display()
                ),
            )),
            Err(error) => Err(Done::fail(
                Reason::NotReady,
                format!(
                    "{} is mounted and a look-up in it failed ({error}): its server does not \
                     answer",
                    mountpoint.display()
                ),
            )),
        }
    }
}
