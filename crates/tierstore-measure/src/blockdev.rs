//! Which block device holds a path, and the device's queue settings (`read_ahead_kb`), which an
//! arm may change for a cell and always puts back.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::Error;

/// The name of the whole block device (`sdb`, `loop1`, not `sdb1`) that holds `path`, found
/// through `<sys>/dev/block/<major>:<minor>` (`sys` is `/sys` in a run).
///
/// # Errors
/// When `path` or the sysfs entry cannot be read.
pub fn device_of(path: &Path, sys: &Path) -> Result<String, Error> {
    let dev = std::fs::metadata(path)
        .map_err(Error::io("stat", path))?
        .dev();
    let link = sys.join(format!(
        "dev/block/{}:{}",
        rustix::fs::major(dev),
        rustix::fs::minor(dev)
    ));
    whole_disk(&link)
}

/// The whole-disk name of the sysfs device directory `link` (a link to it, or the directory).
///
/// # Errors
/// When `link` cannot be resolved or has no name.
pub fn whole_disk(link: &Path) -> Result<String, Error> {
    let dir = std::fs::canonicalize(link).map_err(Error::io("resolve", link))?;
    let disk = if dir.join("partition").exists() {
        dir.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        dir
    };
    disk.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::Invalid(format!("no device name in {}", link.display())))
}

/// The file behind a loop device (`<sys>/block/<dev>/loop/backing_file`), or `None` for a device
/// that is not a loop.
#[must_use]
pub fn backing_file(sys: &Path, dev: &str) -> Option<PathBuf> {
    let text =
        std::fs::read_to_string(sys.join("block").join(dev).join("loop/backing_file")).ok()?;
    let t = text.trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

/// A queue setting of one device: `<sys>/block/<dev>/queue/<name>`.
#[derive(Debug, Clone)]
pub struct QueueSetting {
    path: PathBuf,
}

impl QueueSetting {
    /// The setting `name` of `dev`.
    #[must_use]
    pub fn new(sys: &Path, dev: &str, name: &str) -> Self {
        Self {
            path: sys.join("block").join(dev).join("queue").join(name),
        }
    }

    /// Its value, trimmed.
    ///
    /// # Errors
    /// When it cannot be read.
    pub fn read(&self) -> Result<String, Error> {
        std::fs::read_to_string(&self.path)
            .map(|s| s.trim().to_string())
            .map_err(Error::io("read", &self.path))
    }

    /// Sets it to `value` and returns a guard that puts the old value back when dropped.
    ///
    /// # Errors
    /// When it cannot be read or written.
    pub fn set(&self, value: &str) -> Result<Restore, Error> {
        let old = self.read()?;
        std::fs::write(&self.path, value).map_err(Error::io("write", &self.path))?;
        Ok(Restore {
            path: self.path.clone(),
            old,
        })
    }
}

/// Puts a queue setting back to its old value when dropped (best effort: a failure is reported
/// by [`Restore::restore`] when called explicitly).
#[derive(Debug)]
pub struct Restore {
    path: PathBuf,
    old: String,
}

impl Restore {
    /// Puts the old value back now.
    ///
    /// # Errors
    /// When it cannot be written.
    pub fn restore(self) -> Result<(), Error> {
        let r = std::fs::write(&self.path, &self.old).map_err(Error::io("write", &self.path));
        std::mem::forget(self);
        r
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tierstore_testkit::dir::TestDir;

    fn fake_sys() -> TestDir {
        let d = TestDir::under(std::env::temp_dir(), "sys");
        let p = d.path();
        std::fs::create_dir_all(p.join("devices/pci/block/sdb/sdb1")).unwrap();
        std::fs::write(p.join("devices/pci/block/sdb/sdb1/partition"), "1").unwrap();
        std::fs::create_dir_all(p.join("devices/virtual/block/loop1/loop")).unwrap();
        std::fs::write(
            p.join("devices/virtual/block/loop1/loop/backing_file"),
            "/var/img\n",
        )
        .unwrap();
        std::fs::create_dir_all(p.join("dev/block")).unwrap();
        std::fs::create_dir_all(p.join("block")).unwrap();
        std::os::unix::fs::symlink(p.join("devices/virtual/block/loop1"), p.join("block/loop1"))
            .unwrap();
        std::fs::create_dir_all(p.join("block/sdb/queue")).unwrap();
        std::fs::write(p.join("block/sdb/queue/read_ahead_kb"), "8192\n").unwrap();
        d
    }

    #[test]
    fn whole_disk_of_a_partition_is_its_parent_and_of_a_disk_itself() {
        let s = fake_sys();
        let p = s.path();
        assert_eq!(
            whole_disk(&p.join("devices/pci/block/sdb/sdb1")).unwrap(),
            "sdb"
        );
        assert_eq!(
            whole_disk(&p.join("devices/virtual/block/loop1")).unwrap(),
            "loop1"
        );
        assert!(whole_disk(&p.join("none")).is_err());
        assert!(whole_disk(Path::new("/")).is_err());
    }

    #[test]
    fn device_of_follows_the_dev_block_link() {
        let s = fake_sys();
        let p = s.path();
        let file = p.join("f");
        std::fs::write(&file, "x").unwrap();
        let dev = std::fs::metadata(&file).unwrap().dev();
        std::os::unix::fs::symlink(
            p.join("devices/pci/block/sdb/sdb1"),
            p.join(format!(
                "dev/block/{}:{}",
                rustix::fs::major(dev),
                rustix::fs::minor(dev)
            )),
        )
        .unwrap();
        assert_eq!(device_of(&file, p).unwrap(), "sdb");
        assert!(device_of(&p.join("missing"), p).is_err());
    }

    #[test]
    fn backing_file_of_a_loop_and_of_a_disk() {
        let s = fake_sys();
        assert_eq!(
            backing_file(s.path(), "loop1"),
            Some(PathBuf::from("/var/img"))
        );
        assert_eq!(backing_file(s.path(), "sdb"), None);
        std::fs::write(
            s.path()
                .join("devices/virtual/block/loop1/loop/backing_file"),
            "\n",
        )
        .unwrap();
        assert_eq!(backing_file(s.path(), "loop1"), None);
    }

    #[test]
    fn a_setting_is_put_back_on_drop_and_on_restore() {
        let s = fake_sys();
        let q = QueueSetting::new(s.path(), "sdb", "read_ahead_kb");
        assert_eq!(q.read().unwrap(), "8192");
        {
            let _g = q.set("512").unwrap();
            assert_eq!(q.read().unwrap(), "512");
        }
        assert_eq!(q.read().unwrap(), "8192");
        let g = q.set("128").unwrap();
        g.restore().unwrap();
        assert_eq!(q.read().unwrap(), "8192");
        let missing = QueueSetting::new(s.path(), "sdq", "read_ahead_kb");
        assert!(missing.read().is_err());
        assert!(missing.set("1").is_err());
    }

    #[test]
    fn restore_reports_a_write_failure() {
        let s = fake_sys();
        let q = QueueSetting::new(s.path(), "sdb", "read_ahead_kb");
        let g = q.set("512").unwrap();
        std::fs::remove_dir_all(s.path().join("block/sdb")).unwrap();
        assert!(g.restore().is_err());
        let s2 = fake_sys();
        let q2 = QueueSetting::new(s2.path(), "sdb", "read_ahead_kb");
        let g2 = q2.set("4").unwrap();
        std::fs::write(s2.path().join("block/sdb/queue/read_ahead_kb"), "4").unwrap();
        // A setting whose file is gone: the drop is best effort and does not panic.
        std::fs::remove_dir_all(s2.path().join("block/sdb")).unwrap();
        drop(g2);
    }
}
