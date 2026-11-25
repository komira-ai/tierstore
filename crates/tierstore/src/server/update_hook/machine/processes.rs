//! The processes of the fs units: the main pid systemd has for an instance, and what `/proc` says
//! of the binary a process runs.
//!
//! `/proc/<pid>/exe` is a link the kernel keeps for the process: reading it gives the path the
//! process executed (with ` (deleted)` after it once that name is gone), and following it reaches
//! the file the process runs, whatever its name is now. Executing it runs that file. The files are
//! under a root (`/` on a host) so that a test can give the host a root of its own; nothing here
//! needs `unsafe`.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::SERVICE_BINARY;
use super::super::ports::{Exe, FileId, Instance, PortError, PortKind, Processes};
use super::host::under;
use super::runner::{Call, Runner, checked, run};
use super::units::SystemdUnits;

const KIND: PortKind = PortKind::Host;

/// The processes of the instances of one template, through `systemctl` and `/proc` under a root.
pub struct ProcFiles<'a> {
    units: &'a SystemdUnits<'a>,
    root: PathBuf,
    runner: &'a dyn Runner,
    timeout: Duration,
}

impl<'a> ProcFiles<'a> {
    /// The processes of the instances `units` drives, with `/proc` and [`SERVICE_BINARY`] under
    /// `root` (`/` for the machine itself). A binary asked for its version is run through
    /// `runner` and killed after `timeout`.
    #[must_use]
    pub fn new(
        units: &'a SystemdUnits<'a>,
        root: PathBuf,
        runner: &'a dyn Runner,
        timeout: Duration,
    ) -> ProcFiles<'a> {
        ProcFiles {
            units,
            root,
            runner,
            timeout,
        }
    }

    /// The link `/proc/<pid>/exe` under the root.
    fn exe_link(&self, pid: u32) -> PathBuf {
        under(&self.root, Path::new(&format!("/proc/{pid}/exe")))
    }
}

/// The device and inode of `meta`.
fn file_id(meta: &fs::Metadata) -> FileId {
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

/// The version in what `tierstore --version` printed: the word after the program's name.
fn version_in(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    words.next()?;
    words.next().map(str::to_owned)
}

impl Processes for ProcFiles<'_> {
    fn main_pid(&self, instance: Instance) -> Result<Option<u32>, PortError> {
        self.units.main_pid(instance)
    }

    /// Reads the link `/proc/<pid>/exe` and follows it (`stat`, not `lstat`), to the file the
    /// process runs.
    fn exe(&self, pid: u32) -> Result<Exe, PortError> {
        let link = self.exe_link(pid);
        let failed =
            |error: std::io::Error| PortError::failed(KIND, format!("{}: {error}", link.display()));
        let path = fs::read_link(&link).map_err(failed)?;
        let meta = fs::metadata(&link).map_err(failed)?;
        Ok(Exe {
            path,
            file: file_id(&meta),
        })
    }

    fn installed(&self) -> Result<FileId, PortError> {
        let binary = under(&self.root, Path::new(SERVICE_BINARY));
        let meta = fs::metadata(&binary)
            .map_err(|error| PortError::failed(KIND, format!("{}: {error}", binary.display())))?;
        Ok(file_id(&meta))
    }

    /// Runs `/proc/<pid>/exe --version` as the user the hook runs as, with the environment of
    /// every program the hook runs: a fixed `PATH` and `LC_ALL=C`.
    fn version(&self, pid: u32) -> Result<String, PortError> {
        let link = self.exe_link(pid);
        let call = Call::new(&link.to_string_lossy(), &["--version"], self.timeout);
        let output = checked(run(self.runner, &call, KIND)?, &call, KIND)?;
        let text = String::from_utf8_lossy(&output.stdout);
        version_in(&text).ok_or_else(|| {
            PortError::failed(
                KIND,
                format!("{}: no version in {:?}", call.describe(), text.trim()),
            )
        })
    }
}
