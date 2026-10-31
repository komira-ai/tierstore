//! A scratch directory a unit test owns: created empty, removed when it drops.

use std::ops::Deref;
use std::path::{Path, PathBuf};

pub struct TestDir(PathBuf);

impl TestDir {
    /// The empty directory `base/name`, made fresh (anything a killed run left is removed).
    pub fn under(base: PathBuf, name: &str) -> TestDir {
        let path = base.join(name);
        if path.exists() {
            std::fs::remove_dir_all(&path).expect("remove the old scratch directory");
        }
        std::fs::create_dir_all(&path).expect("create the scratch directory");
        TestDir(path)
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // A directory that cannot be removed is left for the next run to replace.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
