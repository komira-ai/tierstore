//! A dedicated xfs file system on a loop device over an image file: arm B's "loop with
//! `direct-io=on`" device, and the churn arm's fixed-size scratch file system on an HDD (so the
//! drive's own file system, and silo's data on it, keep their headroom).

use std::path::{Path, PathBuf};

use crate::Error;
use crate::run::Runner;

/// A mounted loop file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopFs {
    /// The image file.
    pub image: PathBuf,
    /// The loop device (`/dev/loop7`).
    pub dev: String,
    /// The mount point.
    pub mnt: PathBuf,
}

impl LoopFs {
    /// The device's name without `/dev/` (`loop7`), as `/proc/diskstats` names it.
    #[must_use]
    pub fn name(&self) -> &str {
        self.dev.strip_prefix("/dev/").unwrap_or(&self.dev)
    }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Makes an image of `bytes` (allocated, so it does not grow later) at `image`, attaches it to
/// a loop device (with `direct_io` as `--direct-io`), makes xfs on it and mounts it at `mnt`
/// with `noatime`.
///
/// # Errors
/// When a step fails; the steps already done are undone first.
pub fn setup(
    run: &dyn Runner,
    image: &Path,
    bytes: u64,
    direct_io: bool,
    mnt: &Path,
) -> Result<LoopFs, Error> {
    if image.exists() {
        return Err(Error::Invalid(format!(
            "{} exists: remove it (teardown) first",
            image.display()
        )));
    }
    run.run(&["fallocate", "-l", &bytes.to_string(), &s(image)])?;
    let dio = format!("--direct-io={}", if direct_io { "on" } else { "off" });
    let dev = match run.run(&["losetup", &dio, "--find", "--show", &s(image)]) {
        Ok(out) => out.trim().to_string(),
        Err(e) => {
            let _ = std::fs::remove_file(image);
            return Err(e);
        }
    };
    let fs = LoopFs {
        image: image.to_path_buf(),
        dev,
        mnt: mnt.to_path_buf(),
    };
    let rest = || -> Result<(), Error> {
        run.run(&["mkfs.xfs", "-q", "-f", &fs.dev])?;
        std::fs::create_dir_all(mnt).map_err(Error::io("mkdir", mnt))?;
        run.run(&["mount", "-o", "noatime", &fs.dev, &s(mnt)])?;
        Ok(())
    };
    if let Err(e) = rest() {
        let _ = run.run(&["losetup", "-d", &fs.dev]);
        let _ = std::fs::remove_file(image);
        return Err(e);
    }
    Ok(fs)
}

/// Unmounts, detaches and removes the image. Each step runs even if an earlier one failed; the
/// first failure is returned.
///
/// # Errors
/// The first step that failed.
pub fn teardown(run: &dyn Runner, fs: &LoopFs) -> Result<(), Error> {
    let a = run.run(&["umount", &s(&fs.mnt)]).map(|_| ());
    let b = run.run(&["losetup", "-d", &fs.dev]).map(|_| ());
    let c = std::fs::remove_file(&fs.image).map_err(Error::io("remove", &fs.image));
    a.and(b).and(c)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use tierstore_testkit::dir::TestDir;

    /// A runner that records each command and answers from a script: a command whose first word
    /// is in `fail` fails; `losetup --find` prints `/dev/loop9`; `fallocate` makes the file.
    #[derive(Default)]
    pub struct Script {
        pub seen: Mutex<Vec<String>>,
        pub fail: Vec<&'static str>,
        pub answers: Vec<(&'static str, String)>,
    }

    impl Runner for Script {
        fn run(&self, argv: &[&str]) -> Result<String, Error> {
            let cmd = argv.join(" ");
            self.seen.lock().unwrap().push(cmd.clone());
            if self.fail.contains(&argv[0]) {
                return Err(Error::Command {
                    cmd,
                    detail: "scripted failure".into(),
                });
            }
            if argv[0] == "fallocate" {
                std::fs::write(argv[3], b"").unwrap();
            }
            if let Some((_, a)) = self.answers.iter().find(|(p, _)| cmd.starts_with(p)) {
                return Ok(a.clone());
            }
            if argv[0] == "losetup" && argv.contains(&"--find") {
                return Ok("/dev/loop9\n".into());
            }
            Ok(String::new())
        }
    }

    #[test]
    fn setup_runs_the_steps_in_order_and_teardown_undoes_them() {
        let d = TestDir::under(std::env::temp_dir(), "loopfs");
        let img = d.path().join("img");
        let mnt = d.path().join("mnt");
        let r = Script::default();
        let fs = setup(&r, &img, 1 << 30, true, &mnt).unwrap();
        assert_eq!(fs.dev, "/dev/loop9");
        assert_eq!(fs.name(), "loop9");
        assert!(mnt.is_dir());
        teardown(&r, &fs).unwrap();
        assert!(!img.exists());
        let seen = r.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                format!("fallocate -l 1073741824 {}", img.display()),
                format!("losetup --direct-io=on --find --show {}", img.display()),
                "mkfs.xfs -q -f /dev/loop9".to_string(),
                format!("mount -o noatime /dev/loop9 {}", mnt.display()),
                format!("umount {}", mnt.display()),
                "losetup -d /dev/loop9".to_string(),
            ]
        );
        let odd = LoopFs {
            image: img.clone(),
            dev: "loop3".into(),
            mnt,
        };
        assert_eq!(odd.name(), "loop3");
    }

    #[test]
    fn a_failed_step_undoes_the_earlier_ones() {
        let d = TestDir::under(std::env::temp_dir(), "loopfs");
        let img = d.path().join("img");
        let mnt = d.path().join("mnt");
        for (fail, detached) in [("losetup", false), ("mkfs.xfs", true), ("mount", true)] {
            let r = Script {
                fail: vec![fail],
                ..Script::default()
            };
            let e = setup(&r, &img, 4096, false, &mnt).unwrap_err();
            assert!(e.to_string().contains("scripted failure"), "{fail}: {e}");
            assert!(!img.exists(), "{fail}: the image is removed");
            let seen = r.seen.lock().unwrap().clone();
            assert_eq!(
                seen.last().unwrap() == "losetup -d /dev/loop9",
                detached,
                "{fail}: {seen:?}"
            );
        }
        let r = Script {
            fail: vec!["fallocate"],
            ..Script::default()
        };
        assert!(setup(&r, &img, 4096, false, &mnt).is_err());
        std::fs::write(&img, b"").unwrap();
        let e = setup(&Script::default(), &img, 4096, false, &mnt).unwrap_err();
        assert!(
            e.to_string()
                .ends_with("exists: remove it (teardown) first")
        );
        // A mount point under a file cannot be made.
        std::fs::remove_file(&img).unwrap();
        let file = d.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(setup(&Script::default(), &img, 4096, false, &file.join("m")).is_err());
        assert!(!img.exists());
    }

    #[test]
    fn teardown_runs_every_step_and_returns_the_first_failure() {
        let d = TestDir::under(std::env::temp_dir(), "loopfs");
        let r = Script {
            fail: vec!["umount"],
            ..Script::default()
        };
        let fs = LoopFs {
            image: d.path().join("gone"),
            dev: "/dev/loop9".into(),
            mnt: d.path().join("mnt"),
        };
        let e = teardown(&r, &fs).unwrap_err();
        assert!(e.to_string().starts_with("command `umount"));
        assert_eq!(r.seen.lock().unwrap().len(), 2);
        assert!(
            teardown(&Script::default(), &fs).is_err(),
            "the image is gone"
        );
    }
}
