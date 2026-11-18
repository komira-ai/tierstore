//! A fake machine for the arms' tests: a scratch directory with a sysfs tree and a
//! `/proc/diskstats` for named devices, and a directory per fake drive.

use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

/// A scratch machine.
pub struct Fake {
    pub dir: TestDir,
}

impl Fake {
    /// A machine with block devices `devs`, each with `read_ahead_kb` 8192 and zero counters.
    pub fn new(name: &str, devs: &[&str]) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let mut stats = String::new();
        for (i, d) in devs.iter().enumerate() {
            let q = dir.path().join("sys/block").join(d).join("queue");
            std::fs::create_dir_all(&q).unwrap();
            std::fs::write(q.join("read_ahead_kb"), "8192\n").unwrap();
            stats.push_str(&format!("8 {} {d} 0 0 0 0 0 0 0 0 0 0 0\n", i * 16));
        }
        std::fs::write(dir.path().join("diskstats"), stats).unwrap();
        std::fs::write(dir.path().join("loadavg"), "0.10 0.10 0.10 1/100 1\n").unwrap();
        std::fs::write(dir.path().join("meminfo"), "Cached:  1024 kB\n").unwrap();
        Self { dir }
    }

    pub fn sys(&self) -> PathBuf {
        self.dir.path().join("sys")
    }

    pub fn meminfo(&self) -> PathBuf {
        self.dir.path().join("meminfo")
    }

    pub fn diskstats(&self) -> PathBuf {
        self.dir.path().join("diskstats")
    }

    /// A directory standing for a drive's scratch directory.
    pub fn drive(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Makes the sysfs entry of the device holding `path` resolve to the whole disk `name`.
    pub fn link_dev(&self, path: &Path, name: &str) {
        use std::os::unix::fs::MetadataExt as _;
        let dev = std::fs::metadata(path).unwrap().dev();
        let target = self.sys().join("devices/fake/block").join(name);
        std::fs::create_dir_all(&target).unwrap();
        let link = self.sys().join(format!(
            "dev/block/{}:{}",
            rustix::fs::major(dev),
            rustix::fs::minor(dev)
        ));
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
    }
}

/// Sets a stop flag when dropped, so a test that panics while a server thread runs in its scope
/// still stops that thread (and fails instead of hanging).
pub struct StopOnDrop<'a>(pub &'a std::sync::atomic::AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
