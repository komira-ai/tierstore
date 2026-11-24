//! A build node of a Mac handed over to the next image of its program by SIGUSR2
//! (`docs/design/one-service.md` section 3.4): the process executes itself, keeps its pid, and the
//! new image serves on the same address with the cache's lock. Once as a child of the test, and
//! once as a launchd job sent `launchctl kill SIGUSR2`: launchd keeps the job and its pid, and does
//! not start it again.
//!
//! An integration target of real OS adapters (launchd, `sudo`, `launchctl`): it runs where macOS is
//! tested beside a release (`beta.yml`), never in a pull request. The launchd test needs `sudo -n`
//! and says why it skips without it. Everything it makes is in a scratch directory and one launchd
//! job of its own label, removed at the end; no assertion is about time.

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tierstore_testkit::dir::TestDir;

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// The key file's lines: an access key id and its secret.
const KEY: &str = "ID1\nsecret-0123456789abcdef\n";

/// The file of a build node over a memory upstream, its cache in `dir/cache`, its key file
/// `key`, serving on a loopback port the system picks.
fn node_file(dir: &Path, key: &Path) -> String {
    format!(
        "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\n\
         [server]\nproxy_id = \"exec-handover\"\nbucket = \"b\"\naccess_key_file = \"{}\"\n\
         drain_lb_wait = \"50ms\"\n\n\
         [local_cache]\npath = \"{}\"\nbudget = \"64MiB\"\n\n\
         [cold_store]\nkind = \"memory\"\n\n[[policy]]\nprefix = \"cas/\"\ncache = true\n",
        key.display(),
        dir.join("cache").display(),
    )
}

/// The address of the start line `line` (`tierstore <version> listen=<address> ...`).
fn listen_of(line: &str) -> SocketAddr {
    let listen = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("listen="))
        .unwrap_or_else(|| panic!("a start line with listen=: {line}"));
    listen.parse().unwrap()
}

/// The body of the answer to `GET /readyz` at `addr`, or none if nothing answered.
fn readyz(addr: SocketAddr) -> Option<String> {
    let mut stream = TcpStream::connect(addr).ok()?;
    let ask = b"GET /readyz HTTP/1.1\r\nhost: tier.test\r\nconnection: close\r\n\r\n";
    stream.write_all(ask).ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok()?;
    answer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
}

/// Waits until `/readyz` at `addr` says `ready read-write`: the instance there serves and holds
/// the cache's lock. It fails after a bounded number of asks, none of them timed.
fn read_write_at(addr: SocketAddr) {
    let mut said = None;
    for _ in 0..100_000 {
        said = readyz(addr);
        if said.as_deref() == Some("ready read-write") {
            return;
        }
        std::thread::yield_now();
    }
    panic!("{addr} never said ready read-write; last: {said:?}");
}

/// Asserts that a build node sent SIGUSR2 executes the next image of its program in the same
/// process: the child the test started is still running (the same pid), the new image says its
/// start line on the same standard output, serves on the same address, and holds the cache's lock
/// (`ready read-write`), without a restart; and that SIGTERM then ends it with status 0.
#[test]
fn a_build_node_sent_sigusr2_serves_again_from_the_same_pid() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "exec-handover");
    let key = dir.path().join("key");
    std::fs::write(&key, KEY).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = dir.path().join("node.toml");
    std::fs::write(&config, node_file(dir.path(), &key)).unwrap();
    let mut child = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    let addr = listen_of(&first);
    read_write_at(addr);

    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::USR2).unwrap();
    let mut second = String::new();
    stdout.read_line(&mut second).unwrap();
    assert_eq!(listen_of(&second), addr, "{second}");
    assert!(
        child.try_wait().unwrap().is_none(),
        "the process did not exit"
    );
    read_write_at(addr);

    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
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

/// The scratch directory and the job: booted out and removed when dropped.
struct Scratch {
    dir: PathBuf,
    label: Option<String>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(label) = &self.label {
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
    /// Writes `text` to `name` in the directory, owned by root, with `mode`.
    fn file(&self, name: &str, text: &str, mode: &str) -> PathBuf {
        let staged = std::env::temp_dir().join(format!("{}-{name}", std::process::id()));
        std::fs::write(&staged, text).unwrap();
        let path = self.dir.join(name);
        let (from, to) = (staged.display().to_string(), path.display().to_string());
        sudo_ok("/bin/mv", &[&from, &to]);
        sudo_ok("/usr/sbin/chown", &["root:wheel", &to]);
        sudo_ok("/bin/chmod", &[mode, &to]);
        path
    }
}

/// What the root-owned file `path` holds.
fn read_root(path: &Path) -> String {
    let ran = sudo("/bin/cat", &[&path.display().to_string()]);
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

/// The value of `key` (`<key> = <value>`) in what `launchctl print system/<label>` says.
fn printed(label: &str, key: &str) -> Option<String> {
    let ran = sudo("/bin/launchctl", &["print", &format!("system/{label}")]);
    let text = String::from_utf8_lossy(&ran.stdout).into_owned();
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(str::to_owned))
}

/// The start lines the job's standard output holds, waiting until there are `count` of them; the
/// test fails if the job stops running first, not after a time.
fn start_lines(log: &Path, label: &str, count: usize) -> Vec<String> {
    loop {
        let said = read_root(log);
        let lines: Vec<String> = said
            .lines()
            .filter(|line| line.contains(" listen="))
            .map(str::to_owned)
            .collect();
        if lines.len() >= count {
            return lines;
        }
        let state = printed(label, "state");
        assert!(
            state.as_deref() == Some("running") || said.is_empty(),
            "{label} is {state:?} with {} of {count} start lines: {said}",
            lines.len()
        );
        std::thread::yield_now();
    }
}

/// Asserts what P7's update of a Mac relies on: a launchd job whose process is sent SIGUSR2 by
/// `launchctl kill` and executes the next image of its program keeps its job and its pid (launchd
/// tracks the job by its pid), is not started again (`runs` stays 1, though the job is `KeepAlive`,
/// which would start it again had it exited), and the new image serves on the same address with the
/// cache's lock.
#[test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
fn a_launchd_job_keeps_its_pid_and_is_not_restarted_across_the_exec() {
    if !sudo("/usr/bin/true", &[]).status.success() {
        eprintln!("skipped: sudo -n is not allowed; this test bootstraps a system launchd job");
        return;
    }
    let pid = std::process::id();
    // A system job's program, file and plist must be root's, in a directory root owns all the way
    // up, which the build's scratch directory is not: the place `fs_launchd_integration.rs` uses.
    let dir = PathBuf::from(format!("/private/var/tmp/tierstore-exec-launchd-{pid}"));
    let mut scratch = Scratch {
        dir: dir.clone(),
        label: None,
    };
    let shown = dir.display().to_string();
    for sub in ["", "/bin", "/cache", "/log"] {
        sudo_ok("/bin/mkdir", &[&format!("{shown}{sub}")]);
    }
    let binary = format!("{shown}/bin/tierstore");
    sudo_ok("/bin/cp", &[BIN, &binary]);
    sudo_ok("/bin/chmod", &["0755", &binary]);
    let key = scratch.file("key", KEY, "0600");
    let config = scratch.file("node.toml", &node_file(&dir, &key), "0644");

    let label = format!("tierstore-exec-launchd-{pid}");
    let out = dir.join("log/out.log");
    let words: String = [binary.as_str(), "--config", &config.display().to_string()]
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
         <key>KeepAlive</key><true/>\
         <key>WorkingDirectory</key><string>{shown}</string>\
         <key>StandardOutPath</key><string>{out}</string>\
         <key>StandardErrorPath</key><string>{shown}/log/err.log</string>\
         </dict></plist>\n",
        out = out.display(),
    );
    let plist = scratch.file(&format!("{label}.plist"), &plist, "0644");
    sudo_ok(
        "/bin/launchctl",
        &["bootstrap", "system", &plist.display().to_string()],
    );
    scratch.label = Some(label.clone());
    let target = format!("system/{label}");
    sudo_ok("/bin/launchctl", &["kickstart", &target]);

    let first = start_lines(&out, &label, 1);
    let addr = listen_of(&first[0]);
    read_write_at(addr);
    let job_pid = printed(&label, "pid").expect("the job runs");
    assert_eq!(printed(&label, "runs").as_deref(), Some("1"));

    sudo_ok("/bin/launchctl", &["kill", "SIGUSR2", &target]);
    let lines = start_lines(&out, &label, 2);
    assert_eq!(listen_of(&lines[1]), addr, "{lines:?}");
    read_write_at(addr);
    assert_eq!(printed(&label, "pid"), Some(job_pid), "the same process");
    assert_eq!(
        printed(&label, "runs").as_deref(),
        Some("1"),
        "launchd did not start the job again"
    );
    drop(scratch);
}
