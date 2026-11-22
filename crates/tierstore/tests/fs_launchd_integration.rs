//! `tierstore fs` started by launchd under the identity its packaged job declares: the `UserName`
//! and `GroupName` of `packaging/hostkeeper/units/tierstore-fs.plist`, beside a node's tierstore
//! started under those of `tierstore.plist`. The fill socket answers only a peer of its own user,
//! so a fill the fs process asks for (the root manifest of a tree a client attaches) is made by
//! the store only when the two jobs run as one user; any other user's is `refused`.
//!
//! An integration target of real OS adapters (launchd, `sudo`, the account database): it runs
//! where macOS is tested beside a release, never in a pull request. It needs `sudo -n` and the
//! account the plists name (the macOS package makes `_tierstore`), and says why it skips when
//! either is missing. Everything it makes is in a scratch directory and two launchd jobs of its
//! own labels, removed at the end; no assertion is about time.

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The words the fs process logs once it serves.
const SERVING: &str = "tierstore fs: serving NFS on";

/// A tree the store does not have: its root manifest is one fill, which the store makes and its
/// upstream (memory, empty) answers 404.
const ABSENT_TREE: &str = "abababababababababababababababababababababababababababababababab";

/// The packaged plist `name`.
fn packaged(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/hostkeeper/units")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

/// The string the plist `text` gives `key`.
fn value(text: &str, key: &str) -> String {
    let after = text
        .split_once(&format!("<key>{key}</key>"))
        .unwrap_or_else(|| panic!("the plist has no {key}"))
        .1;
    let start = after.find("<string>").unwrap() + "<string>".len();
    let end = after.find("</string>").unwrap();
    after[start..end].to_owned()
}

/// Runs `program` with `args` through `sudo -n`.
fn sudo(program: &str, args: &[&str]) -> Output {
    Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg(program)
        .args(args)
        .output()
        .unwrap()
}

/// Runs `program` with `args` through `sudo -n`, and asserts it succeeded.
fn sudo_ok(program: &str, args: &[&str]) {
    let ran = sudo(program, args);
    assert!(
        ran.status.success(),
        "sudo {program} {args:?}: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
}

/// Why this machine cannot run the test, if it cannot.
fn missing(user: &str) -> Option<String> {
    if !sudo("/usr/bin/true", &[]).status.success() {
        return Some("sudo -n is not allowed".to_owned());
    }
    let known = Command::new("/usr/bin/id").arg(user).output().unwrap();
    (!known.status.success()).then(|| format!("the account {user} is not on this machine"))
}

/// The scratch directory and the jobs: booted out and removed when dropped.
struct Scratch {
    dir: PathBuf,
    labels: Vec<String>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for label in &self.labels {
            let target = format!("system/{label}");
            let _ = sudo("/bin/launchctl", &["bootout", &target]);
            // A bootout of a job that is still draining returns before the job is gone; launchd
            // removes it when the process exits (its ExitTimeOut bounds that).
            while sudo("/bin/launchctl", &["print", &target]).status.success() {
                std::thread::yield_now();
            }
        }
        let _ = sudo("/bin/rm", &["-r", &self.dir.display().to_string()]);
    }
}

impl Scratch {
    /// Writes `text` to `name` in the directory, owned by `owner`, with `mode`.
    fn file(&self, name: &str, text: &str, owner: &str, mode: &str) -> PathBuf {
        let staged = std::env::temp_dir().join(format!("{}-{name}", std::process::id()));
        std::fs::write(&staged, text).unwrap();
        let path = self.dir.join(name);
        let (from, to) = (staged.display().to_string(), path.display().to_string());
        sudo_ok("/bin/mv", &[&from, &to]);
        sudo_ok("/usr/sbin/chown", &[owner, &to]);
        sudo_ok("/bin/chmod", &[mode, &to]);
        path
    }

    /// Bootstraps a system job `label` running `arguments` as `user`:`group`, its standard
    /// error in `log/<label>.log`, and starts it.
    fn job(&mut self, label: &str, arguments: &[String], (user, group): (&str, &str)) -> PathBuf {
        let log = self.dir.join("log").join(format!("{label}.log"));
        let words: String = arguments
            .iter()
            .map(|word| format!("<string>{word}</string>"))
            .collect();
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\
             <key>Label</key><string>{label}</string>\
             <key>ProgramArguments</key><array>{words}</array>\
             <key>UserName</key><string>{user}</string>\
             <key>GroupName</key><string>{group}</string>\
             <key>WorkingDirectory</key><string>{dir}</string>\
             <key>StandardErrorPath</key><string>{log}</string>\
             </dict></plist>\n",
            dir = self.dir.display(),
            log = log.display(),
        );
        let path = self.file(&format!("{label}.plist"), &plist, "root:wheel", "0644");
        sudo_ok(
            "/bin/launchctl",
            &["bootstrap", "system", &path.display().to_string()],
        );
        self.labels.push(label.to_owned());
        sudo_ok("/bin/launchctl", &["kickstart", &format!("system/{label}")]);
        log
    }
}

/// What the root-owned log `path` says.
fn read_log(path: &Path) -> String {
    let ran = sudo("/bin/cat", &[&path.display().to_string()]);
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

/// Waits until `log` says one of `lines`, and gives what it says; the test fails if the job
/// ended first (`label` no longer runs), not after a time.
fn until_said(log: &Path, label: &str, lines: &[&str]) -> String {
    loop {
        let said = read_log(log);
        if lines.iter().any(|line| said.contains(line)) {
            return said;
        }
        let state = sudo("/bin/launchctl", &["print", &format!("system/{label}")]);
        let printed = String::from_utf8_lossy(&state.stdout);
        assert!(
            printed.contains("state = running") || said.is_empty(),
            "{label} ended before it said any of {lines:?}: {said}\n{printed}"
        );
        std::thread::yield_now();
    }
}

/// Asserts that `tierstore fs`, started by launchd with the `UserName` and `GroupName` of its
/// packaged job, runs as that user, and that a fill it asks of a node's tierstore started by
/// launchd with the identity of the store's packaged job is made by the store (the attach of a
/// tree the empty upstream does not have fails with the store's `upstream 404`, not `refused`):
/// the fs job's identity is one the fill socket admits. Catches a packaged fs job whose user is not
/// the store's (root, before this test's release), which has every fill refused.
#[test]
fn the_packaged_fs_identity_is_admitted_by_the_store_fill_socket() {
    let fs_plist = packaged("tierstore-fs.plist");
    let store_plist = packaged("tierstore.plist");
    let fs_id = (value(&fs_plist, "UserName"), value(&fs_plist, "GroupName"));
    let store_id = (
        value(&store_plist, "UserName"),
        value(&store_plist, "GroupName"),
    );
    if let Some(why) = missing(&fs_id.0) {
        eprintln!("skipped: {why}; this test needs sudo -n and the packaged account");
        return;
    }
    let pid = std::process::id();
    let dir = PathBuf::from(format!("/private/var/tmp/tierstore-fs-launchd-{pid}"));
    let mut scratch = Scratch {
        dir: dir.clone(),
        labels: Vec::new(),
    };
    let shown = dir.display().to_string();
    for sub in ["", "/bin", "/cache", "/run", "/mnt", "/fs-run", "/log"] {
        sudo_ok("/bin/mkdir", &[&format!("{shown}{sub}")]);
    }
    sudo_ok("/bin/chmod", &["0755", &shown, &format!("{shown}/bin")]);
    let owner = format!("{}:{}", store_id.0, store_id.1);
    for sub in ["/cache", "/run", "/mnt", "/fs-run", "/log"] {
        sudo_ok("/usr/sbin/chown", &[&owner, &format!("{shown}{sub}")]);
    }
    let binary = format!("{shown}/bin/tierstore");
    sudo_ok("/bin/cp", &[env!("CARGO_BIN_EXE_tierstore"), &binary]);
    sudo_ok("/bin/chmod", &["0755", &binary]);

    let key = scratch.file(
        "access.key",
        "ID1\nsecret-0123456789abcdef\n",
        &owner,
        "0600",
    );
    let store_config = format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\
         [server]\nproxy_id = \"tierstore-fs-launchd-{pid}\"\nbucket = \"b\"\n\
         access_key_file = \"{}\"\n\
         [local_cache]\npath = \"{shown}/cache\"\nfill_socket = \"{shown}/run/fill.sock\"\n\
         budget = \"64MiB\"\n\
         [cold_store]\nkind = \"memory\"\n\
         [[policy]]\nprefix = \"cas/\"\ncache = true\n",
        key.display()
    );
    let store_file = scratch.file("store.toml", &store_config, &owner, "0644");
    let uid = Command::new("/usr/bin/id")
        .args(["-u", &fs_id.0])
        .output()
        .unwrap();
    let gid = Command::new("/usr/bin/id")
        .args(["-g", &fs_id.0])
        .output()
        .unwrap();
    let fs_config = format!(
        "[fs]\nmountpoint = \"{shown}/mnt\"\nruntime_dir = \"{shown}/fs-run\"\n\
         client_group = \"everyone\"\nowner = \"{}:{}\"\nfill_socket = \"{shown}/run/fill.sock\"\n\
         cache_path = \"{shown}/cache\"\n[fs.nfs]\nlisten = \"127.0.0.1:0\"\nmount = false\n",
        String::from_utf8_lossy(&uid.stdout).trim(),
        String::from_utf8_lossy(&gid.stdout).trim(),
    );
    let fs_file = scratch.file("fs.toml", &fs_config, &owner, "0644");

    let store_label = format!("tierstore-fs-launchd-store-{pid}");
    let store_args = [
        binary.clone(),
        "--config".to_owned(),
        store_file.display().to_string(),
    ];
    let store_log = scratch.job(&store_label, &store_args, (&store_id.0, &store_id.1));
    until_said(&store_log, &store_label, &["the fill socket is at"]);

    let fs_label = format!("tierstore-fs-launchd-fs-{pid}");
    let fs_args = [
        binary.clone(),
        "fs".to_owned(),
        "--config".to_owned(),
        fs_file.display().to_string(),
    ];
    let fs_log = scratch.job(&fs_label, &fs_args, (&fs_id.0, &fs_id.1));
    until_said(&fs_log, &fs_label, &[SERVING]);
    // `everyone` is the control socket's group, so this test, a member, may attach.
    let control = dir.join("fs-run/control.sock");
    let mut stream = std::os::unix::net::UnixStream::connect(&control).unwrap();
    stream
        .write_all(format!("attach {ABSENT_TREE} for run-1\n").as_bytes())
        .unwrap();
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer).unwrap();
    assert!(answer.contains("\"ok\":false"), "{answer}");
    assert!(
        answer.contains("upstream 404"),
        "the store made the fill: {answer}"
    );
    assert!(!answer.contains("refused"), "{answer}");

    let printed = sudo("/bin/launchctl", &["print", &format!("system/{fs_label}")]);
    let printed = String::from_utf8_lossy(&printed.stdout);
    let pid_of = printed
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .expect("the fs job runs")
        .to_owned();
    let user = Command::new("/bin/ps")
        .args(["-o", "user=", "-p", &pid_of])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&user.stdout).trim(), fs_id.0);
    drop(scratch);
}
