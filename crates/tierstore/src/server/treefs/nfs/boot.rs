//! The epoch of the machine's boot: the word that tells a table of tree numbers, and a handle, of
//! this boot's mount from one of an earlier boot's (`docs/design/virtual-build-fs.md` section 7.5).
//! A reboot takes the NFS mount with it, so a table of an earlier boot belongs to a mount no
//! client holds; a restart of the server keeps the boot, and so the table and the handles.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::error::NfsError;

/// Where the boot's identity is read from.
pub(super) struct BootSource {
    /// A file that holds it, where the system has one (Linux: `/proc/sys/kernel/random/boot_id`).
    file: PathBuf,
    /// The command that prints it otherwise (macOS: `sysctl -n kern.bootsessionuuid`).
    program: String,
    args: Vec<String>,
}

impl BootSource {
    /// The sources of this machine.
    pub(super) fn system() -> Self {
        Self::of(
            Path::new("/proc/sys/kernel/random/boot_id"),
            "sysctl",
            &["-n", "kern.bootsessionuuid"],
        )
    }

    /// A source whose command prints a fixed identity: for the tests of the process.
    #[cfg(test)]
    pub(super) fn fixed() -> Self {
        Self::of(Path::new("/nonexistent/boot_id"), "echo", &["boot-1"])
    }

    /// The identity read from `file`, or, when there is none, from what `program` prints.
    pub(super) fn of(file: &Path, program: &str, args: &[&str]) -> Self {
        Self {
            file: file.to_owned(),
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    /// The epoch of this boot: a hash of its identity that is never 0 (0 is the epoch of a table
    /// that has none).
    ///
    /// # Errors
    /// [`NfsError::Boot`]: the identity could not be read, or is empty.
    pub(super) fn epoch(&self) -> Result<u32, NfsError> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(_) => self.printed()?,
        };
        let id = text.trim();
        if id.is_empty() {
            return Err(NfsError::Boot("the boot's identity is empty".to_owned()));
        }
        Ok(epoch_of(id))
    }

    /// What the command prints.
    fn printed(&self) -> Result<String, NfsError> {
        let out = Command::new(&self.program)
            .args(&self.args)
            .output()
            .map_err(|why| NfsError::Boot(format!("{}: {why}", self.program)))?;
        if !out.status.success() {
            return Err(NfsError::Boot(format!(
                "{} {}",
                self.program,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// The 32-bit FNV-1a hash of `id`, and 1 where that is 0.
fn epoch_of(id: &str) -> u32 {
    let hash = id.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    hash.max(1)
}

#[cfg(test)]
#[path = "boot_tests.rs"]
mod tests;
