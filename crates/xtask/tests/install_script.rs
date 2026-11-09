//! `crates/tierstore/site/static/install.sh`, the script served at https://tierstore.dev/install.sh, run
//! with `sh` against a fake release tree and fake `curl`, `uname`, `id` and `sudo` on a scratch `PATH`
//! (no network, no host directory is touched).
//!
//! Each test builds a `releases/` tree in a scratch directory: `latest.json`, and for a version its
//! `SHA256SUMS` and the platform tarballs. The fake `curl` answers a request for the test's base URL from
//! that tree (a file that is not there is a 404) and records every URL it was asked for.
//!
//! Catches:
//! - a platform mapped to the wrong tarball (Linux x86_64 and amd64, Linux aarch64 and arm64, macOS arm64);
//! - an unsupported platform that downloads or installs anything, or whose message does not name Homebrew and apt;
//! - a tarball installed although its SHA-256 differs from `SHA256SUMS`, or `SHA256SUMS` does not list it;
//! - `--version` ignored (the newest read instead), a version that is not a version used in a URL;
//! - `latest.json` read only when it is pretty-printed, or only when it is compact;
//! - `--prefix` not honoured, the default prefix not `$HOME/.local/bin`, the prefix not made;
//! - `sudo` used without `--system`, used for a directory the user can write, or not used when it is needed;
//! - `shasum -a 256` not used when `sha256sum` is missing, and an install with neither;
//! - the binary run with anything but `--version`, or a message that does not say how to start it;
//! - a temporary directory left behind after a success or a failure.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tierstore_testkit::dir::TestDir;

const BASE: &str = "http://releases.test";
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tierstore/site/static/install.sh"
);
/// What the script uses besides the shell's builtins and the fakes.
const TOOLS: &[&str] = &[
    "tar", "gzip", "mktemp", "cat", "tr", "sed", "awk", "cut", "install", "mkdir", "rm", "rmdir",
];

/// A tool of the host, found in the directories the system's tools are in (the test reads no environment variable).
fn which(name: &str) -> PathBuf {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is not in /usr/bin, /bin or /usr/local/bin"))
}

/// The host's SHA-256 command line: `sha256sum` where it exists (Linux), else `shasum -a 256` (macOS, which has no
/// `sha256sum`). The fixture's `sha256sum` and the `shasum` stand-in run it, so the script sees the same tool on both.
fn host_sha256() -> String {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join("sha256sum"))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| format!("{} -a 256", which("shasum").display()))
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A tarball as `cargo xtask package` writes it: `tierstore` and the licences at the top.
fn tarball(version: &str) -> Vec<u8> {
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let script = format!("#!/bin/sh\necho \"$*\" >> \"$0.calls\"\necho 'tierstore {version}'\n");
    let mut add = |name: &str, body: &[u8], mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        tar.append_data(&mut h, name, body).unwrap();
    };
    add("tierstore", script.as_bytes(), 0o755);
    add("THIRD-PARTY-LICENSES.txt", b"licences\n", 0o644);
    tar.into_inner().unwrap().finish().unwrap()
}

struct Run {
    out: Output,
}

impl Run {
    fn ok(&self) -> bool {
        self.out.status.success()
    }
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out.stdout).into_owned()
    }
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.out.stderr).into_owned()
    }
}

struct Fixture {
    dir: TestDir,
}

impl Fixture {
    /// An empty world: no releases, a `uname` that says Linux x86_64, `sha256sum` available.
    fn new(name: &str) -> Self {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        for d in ["releases", "fakes", "tools", "home", "tmp"] {
            fs::create_dir(dir.join(d)).unwrap();
        }
        for t in TOOLS {
            symlink(which(t), dir.join("tools").join(t)).unwrap();
        }
        let digest = dir.join("tools/sha256sum");
        fs::write(
            &digest,
            format!("#!/bin/sh\nexec {} \"$@\"\n", host_sha256()),
        )
        .unwrap();
        fs::set_permissions(&digest, fs::Permissions::from_mode(0o755)).unwrap();
        let f = Self { dir };
        f.platform("Linux", "x86_64");
        f.fake("id", "echo 1000");
        let root = f.dir.join("releases/..");
        f.fake(
            "curl",
            &format!(
                r#"out=; url=
while [ $# -gt 0 ]; do
  case $1 in -o) out=$2; shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac
done
echo "$url" >> {log}
case $url in
  {BASE}/*) f={root}/${{url#{BASE}/}} ;;
  *) echo "unexpected $url" >&2; exit 6 ;;
esac
[ -f "$f" ] || exit 22
cat "$f" > "$out""#,
                log = f.dir.join("curl.log").display(),
                root = root.display(),
            ),
        );
        f
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn fake(&self, name: &str, body: &str) {
        let p = self.path(&format!("fakes/{name}"));
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn platform(&self, os: &str, arch: &str) {
        self.fake(
            "uname",
            &format!("case $1 in -s) echo {os} ;; -m) echo {arch} ;; esac"),
        );
    }

    /// Publishes `version` with the given platforms; `SHA256SUMS` lists the true digests.
    fn release(&self, version: &str, platforms: &[&str]) {
        let dir = self.path(&format!("releases/{version}"));
        fs::create_dir_all(&dir).unwrap();
        let mut sums = String::new();
        for p in platforms {
            let name = format!("tierstore-{version}-{p}.tar.gz");
            let bytes = tarball(version);
            sums.push_str(&format!("{}  {name}\n", sha(&bytes)));
            fs::write(dir.join(name), bytes).unwrap();
        }
        fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    }

    fn latest(&self, json: &str) {
        fs::write(self.path("releases/latest.json"), json).unwrap();
    }

    /// The URLs the fake curl was asked for, in order.
    fn requested(&self) -> Vec<String> {
        fs::read_to_string(self.path("curl.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn run(&self, args: &[&str]) -> Run {
        self.run_with(args, true)
    }

    fn run_with(&self, args: &[&str], hash_tools: bool) -> Run {
        let tools = self.path("tools/sha256sum");
        if !hash_tools && tools.exists() {
            fs::remove_file(&tools).unwrap();
        }
        let path = format!(
            "{}:{}",
            self.path("fakes").display(),
            self.path("tools").display()
        );
        let out = Command::new("/bin/sh")
            .arg(SCRIPT)
            .args(["--base-url", BASE])
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
            .output()
            .unwrap();
        Run { out }
    }

    fn leftovers(&self) -> Vec<String> {
        fs::read_dir(self.path("tmp"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

#[test]
fn each_platform_gets_its_own_tarball_and_the_binary_lands_in_the_prefix() {
    for (os, arch, platform) in [
        ("Linux", "x86_64", "linux-x86_64"),
        ("Linux", "amd64", "linux-x86_64"),
        ("Linux", "aarch64", "linux-aarch64"),
        ("Linux", "arm64", "linux-aarch64"),
        ("Darwin", "arm64", "macos-arm64"),
    ] {
        let f = Fixture::new("install-platform");
        f.platform(os, arch);
        f.release("1.2.3", &["linux-x86_64", "linux-aarch64", "macos-arm64"]);
        f.latest(r#"{"version": "1.2.3"}"#);
        let prefix = f.path("bin");
        let r = f.run(&["--prefix", prefix.to_str().unwrap()]);
        assert!(r.ok(), "{os} {arch}: {}", r.stderr());
        assert_eq!(
            f.requested(),
            [
                format!("{BASE}/releases/latest.json"),
                format!("{BASE}/releases/1.2.3/SHA256SUMS"),
                format!("{BASE}/releases/1.2.3/tierstore-1.2.3-{platform}.tar.gz"),
            ],
            "{os} {arch}"
        );
        let bin = prefix.join("tierstore");
        assert_eq!(
            fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read(&bin).unwrap(), {
            let script = "#!/bin/sh\necho \"$*\" >> \"$0.calls\"\necho 'tierstore 1.2.3'\n";
            script.as_bytes().to_vec()
        });
        assert!(
            r.stdout().contains(&format!("for {platform}")),
            "{}",
            r.stdout()
        );
        assert!(f.leftovers().is_empty(), "{:?}", f.leftovers());
    }
}

#[test]
fn an_unsupported_platform_is_refused_before_any_download_and_names_the_alternatives() {
    for (os, arch) in [
        ("Darwin", "x86_64"),
        ("Linux", "riscv64"),
        ("Linux", "i686"),
        ("FreeBSD", "amd64"),
    ] {
        let f = Fixture::new("install-unsupported");
        f.release("1.2.3", &["linux-x86_64", "linux-aarch64", "macos-arm64"]);
        f.latest(r#"{"version": "1.2.3"}"#);
        let prefix = f.path("bin");
        f.platform(os, arch);
        let r = f.run(&["--prefix", prefix.to_str().unwrap()]);
        assert!(!r.ok(), "{os} {arch}");
        let err = r.stderr();
        assert!(
            err.contains(&format!("no tierstore build for {os} {arch}")),
            "{err}"
        );
        assert!(
            err.contains("brew install komira-ai/tap/tierstore"),
            "{err}"
        );
        assert!(err.contains("sudo apt install tierstore"), "{err}");
        assert!(f.requested().is_empty(), "{os} {arch}: {:?}", f.requested());
        assert!(!exists(&prefix));
    }
}

#[test]
fn a_tarball_whose_digest_differs_from_sha256sums_is_refused_and_nothing_is_installed() {
    let f = Fixture::new("install-mismatch");
    f.release("1.2.3", &["linux-x86_64"]);
    let name = "tierstore-1.2.3-linux-x86_64.tar.gz";
    let sums = f.path("releases/1.2.3/SHA256SUMS");
    fs::write(&sums, format!("{}  {name}\n", sha(b"some other bytes"))).unwrap();
    let prefix = f.path("bin");
    let r = f.run(&["--version", "1.2.3", "--prefix", prefix.to_str().unwrap()]);
    assert!(!r.ok());
    let err = r.stderr();
    assert!(
        err.contains(&format!("checksum mismatch for {name}")),
        "{err}"
    );
    assert!(err.contains(&sha(b"some other bytes")), "{err}");
    assert!(err.contains("nothing was installed"), "{err}");
    assert!(!exists(&prefix.join("tierstore")));
    assert!(f.leftovers().is_empty(), "{:?}", f.leftovers());
}

#[test]
fn a_tarball_that_sha256sums_does_not_list_is_refused() {
    let f = Fixture::new("install-unlisted");
    f.release("1.2.3", &["linux-x86_64"]);
    // The sums list another platform's file only; a `*` before the name is the binary-mode marker.
    fs::write(
        f.path("releases/1.2.3/SHA256SUMS"),
        format!("{}  tierstore-1.2.3-macos-arm64.tar.gz\n", sha(b"x")),
    )
    .unwrap();
    let prefix = f.path("bin");
    let r = f.run(&["--version", "1.2.3", "--prefix", prefix.to_str().unwrap()]);
    assert!(!r.ok());
    assert!(
        r.stderr()
            .contains("lists no tierstore-1.2.3-linux-x86_64.tar.gz"),
        "{}",
        r.stderr()
    );
    assert!(!exists(&prefix.join("tierstore")));
}

#[test]
fn a_binary_mode_marker_in_sha256sums_is_read() {
    let f = Fixture::new("install-star");
    f.release("1.2.3", &["linux-x86_64"]);
    let name = "tierstore-1.2.3-linux-x86_64.tar.gz";
    let bytes = fs::read(f.path(&format!("releases/1.2.3/{name}"))).unwrap();
    fs::write(
        f.path("releases/1.2.3/SHA256SUMS"),
        format!("{} *{name}\n", sha(&bytes)),
    )
    .unwrap();
    let prefix = f.path("bin");
    let r = f.run(&["--version", "1.2.3", "--prefix", prefix.to_str().unwrap()]);
    assert!(r.ok(), "{}", r.stderr());
}

#[test]
fn version_installs_that_version_and_never_reads_latest_json() {
    let f = Fixture::new("install-version");
    f.release("1.0.0", &["linux-x86_64"]);
    f.release("2.0.0", &["linux-x86_64"]);
    f.latest(r#"{"version": "2.0.0"}"#);
    let prefix = f.path("bin");
    let r = f.run(&["--version", "1.0.0", "--prefix", prefix.to_str().unwrap()]);
    assert!(r.ok(), "{}", r.stderr());
    assert_eq!(
        f.requested(),
        [
            format!("{BASE}/releases/1.0.0/SHA256SUMS"),
            format!("{BASE}/releases/1.0.0/tierstore-1.0.0-linux-x86_64.tar.gz"),
        ]
    );
    assert!(
        r.stdout().contains("Installing tierstore 1.0.0"),
        "{}",
        r.stdout()
    );
}

#[test]
fn a_version_that_is_not_a_version_is_refused_before_any_request() {
    for bad in [
        "../x",
        "1.0/../..",
        "v1.0.0",
        "1.0.0 ",
        "1;rm",
        "",
        "latest",
    ] {
        let f = Fixture::new("install-badversion");
        let prefix = f.path("bin");
        let r = f.run(&["--version", bad, "--prefix", prefix.to_str().unwrap()]);
        // An empty --version means "the newest", which has no latest.json here and fails on the download instead.
        assert!(!r.ok(), "{bad:?}");
        if !bad.is_empty() {
            assert!(
                r.stderr().contains("is not a version"),
                "{bad:?}: {}",
                r.stderr()
            );
            assert!(f.requested().is_empty(), "{bad:?}");
        }
    }
}

#[test]
fn latest_json_is_read_pretty_printed_or_compact_and_unknown_keys_are_ignored() {
    for json in [
        "{\n  \"version\": \"3.4.5\",\n  \"tag\": \"v3.4.5\",\n  \"files\": {}\n}\n",
        r#"{"tag":"v3.4.5","version":"3.4.5","commit":"x","files":{"a":{"sha256":"y","size":0}},"extra":[1]}"#,
    ] {
        let f = Fixture::new("install-latest");
        f.release("3.4.5", &["linux-x86_64"]);
        f.latest(json);
        let prefix = f.path("bin");
        let r = f.run(&["--prefix", prefix.to_str().unwrap()]);
        assert!(r.ok(), "{json}: {}", r.stderr());
        assert!(
            r.stdout().contains("Installing tierstore 3.4.5"),
            "{}",
            r.stdout()
        );
    }
}

#[test]
fn a_latest_json_without_a_version_is_refused() {
    let f = Fixture::new("install-latest-empty");
    f.latest(r#"{"tag": "v1"}"#);
    let r = f.run(&["--prefix", f.path("bin").to_str().unwrap()]);
    assert!(!r.ok());
    assert!(r.stderr().contains("names no version"), "{}", r.stderr());
}

#[test]
fn a_failed_download_installs_nothing_and_leaves_no_temporary_files() {
    let f = Fixture::new("install-404");
    f.latest(r#"{"version": "9.9.9"}"#);
    let prefix = f.path("bin");
    let r = f.run(&["--prefix", prefix.to_str().unwrap()]);
    assert!(!r.ok());
    assert!(
        r.stderr().contains(&format!(
            "download failed: {BASE}/releases/9.9.9/SHA256SUMS"
        )),
        "{}",
        r.stderr()
    );
    assert!(!exists(&prefix));
    assert!(f.leftovers().is_empty(), "{:?}", f.leftovers());
}

#[test]
fn a_tarball_without_the_binary_is_refused() {
    let f = Fixture::new("install-nobinary");
    let dir = f.path("releases/1.2.3");
    fs::create_dir_all(&dir).unwrap();
    let name = "tierstore-1.2.3-linux-x86_64.tar.gz";
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let mut h = tar::Header::new_gnu();
    h.set_size(1);
    h.set_mode(0o644);
    h.set_cksum();
    tar.append_data(&mut h, "README", &b"x"[..]).unwrap();
    let bytes = tar.into_inner().unwrap().finish().unwrap();
    fs::write(dir.join(name), &bytes).unwrap();
    fs::write(dir.join("SHA256SUMS"), format!("{}  {name}\n", sha(&bytes))).unwrap();
    let prefix = f.path("bin");
    let r = f.run(&["--version", "1.2.3", "--prefix", prefix.to_str().unwrap()]);
    assert!(!r.ok());
    assert!(
        r.stderr().contains("holds no 'tierstore' file"),
        "{}",
        r.stderr()
    );
    assert!(!exists(&prefix.join("tierstore")));
}

#[test]
fn the_default_prefix_is_home_local_bin_and_is_made() {
    let f = Fixture::new("install-default-prefix");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    let r = f.run(&[]);
    assert!(r.ok(), "{}", r.stderr());
    assert!(f.path("home/.local/bin/tierstore").is_file());
    assert!(
        r.stdout().contains("Note: ") && r.stdout().contains("is not on your PATH"),
        "{}",
        r.stdout()
    );
}

#[test]
fn a_prefix_on_the_path_gets_no_path_note() {
    let f = Fixture::new("install-on-path");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    let prefix = f.path("tools");
    let r = f.run(&["--prefix", prefix.to_str().unwrap()]);
    assert!(r.ok(), "{}", r.stderr());
    assert!(!r.stdout().contains("not on your PATH"), "{}", r.stdout());
}

#[test]
fn the_binary_is_asked_its_version_and_nothing_else_and_the_output_says_how_to_start() {
    let f = Fixture::new("install-not-started");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    let prefix = f.path("bin");
    let r = f.run(&["--prefix", prefix.to_str().unwrap()]);
    assert!(r.ok(), "{}", r.stderr());
    let calls = fs::read_to_string(prefix.join("tierstore.calls")).unwrap();
    assert_eq!(calls, "--version\n");
    let out = r.stdout();
    assert!(
        out.contains(&format!(
            "Installed {}/tierstore (tierstore 1.2.3)",
            prefix.display()
        )),
        "{out}"
    );
    assert!(out.contains("tierstore is not running"), "{out}");
    assert!(
        out.contains("write a configuration file, then enable the service"),
        "{out}"
    );
    assert!(!out.contains("--check-config"), "{out}");
}

#[test]
fn shasum_is_used_when_sha256sum_is_missing_and_no_tool_at_all_is_refused() {
    let f = Fixture::new("install-shasum");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    f.fake(
        "shasum",
        &format!(
            "[ \"$1 $2\" = '-a 256' ] || exit 2\nshift 2\nexec {} \"$@\"",
            host_sha256()
        ),
    );
    let prefix = f.path("bin");
    let r = f.run_with(&["--prefix", prefix.to_str().unwrap()], false);
    assert!(r.ok(), "{}", r.stderr());
    assert!(prefix.join("tierstore").is_file());

    // With the digest tool gone as well, the script refuses before it downloads.
    fs::remove_file(f.path("fakes/shasum")).unwrap();
    let other = f.path("bin2");
    let r = f.run_with(&["--prefix", other.to_str().unwrap()], false);
    assert!(!r.ok());
    assert!(
        r.stderr().contains("sha256sum or shasum is needed"),
        "{}",
        r.stderr()
    );
    assert!(!exists(&other));
}

/// A prefix that cannot be made: its parent is a regular file.
fn unmakeable(f: &Fixture) -> PathBuf {
    let file = f.path("a-file");
    fs::write(&file, "x").unwrap();
    file.join("bin")
}

#[test]
fn sudo_is_used_with_system_when_the_directory_cannot_be_written_and_only_then() {
    let f = Fixture::new("install-sudo");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    let log = f.path("sudo.log");
    f.fake("sudo", &format!("echo \"$*\" >> {}", log.display()));
    let prefix = unmakeable(&f);
    let p = prefix.to_str().unwrap();

    // Without --system there is no sudo: the script says so and stops.
    let r = f.run(&["--prefix", p]);
    assert!(!r.ok());
    assert!(
        r.stderr()
            .contains("is not writable; give another --prefix, or --system"),
        "{}",
        r.stderr()
    );
    assert!(!exists(&log));

    // A directory the user can write needs no sudo even with --system.
    let writable = f.path("mine");
    fs::create_dir(&writable).unwrap();
    let r = f.run(&["--system", "--prefix", writable.to_str().unwrap()]);
    assert!(r.ok(), "{}", r.stderr());
    assert!(!exists(&log));
    assert!(writable.join("tierstore").is_file());

    // With --system and a directory the user cannot make, the placing is done by sudo (the fake only logs it).
    let r = f.run(&["--system", "--prefix", p]);
    assert!(r.ok(), "{}", r.stderr());
    let calls = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.len(), 2, "{calls}");
    assert_eq!(lines[0], format!("mkdir -p {p}"));
    assert!(lines[1].starts_with("install -m 0755 "), "{calls}");
    assert!(
        lines[1].ends_with(&format!("/tierstore {p}/tierstore")),
        "{calls}"
    );
    assert!(r.stdout().contains("using sudo"), "{}", r.stdout());
    assert!(f.leftovers().is_empty(), "{:?}", f.leftovers());
}

#[test]
fn system_without_sudo_or_for_root_behaves() {
    let f = Fixture::new("install-sudo-missing");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    let prefix = unmakeable(&f);
    let p = prefix.to_str().unwrap();
    let r = f.run(&["--system", "--prefix", p]);
    assert!(!r.ok());
    assert!(r.stderr().contains("sudo was not found"), "{}", r.stderr());

    // Root never needs sudo: the placing is attempted directly, and fails on its own error.
    f.fake("id", "echo 0");
    f.fake("sudo", "echo called >> sudo.log; exit 0");
    let r = f.run(&["--system", "--prefix", p]);
    assert!(!r.ok());
    assert!(
        r.stderr()
            .contains("is not writable; give another --prefix"),
        "{}",
        r.stderr()
    );
}

#[test]
fn system_defaults_the_prefix_to_usr_local_bin() {
    let f = Fixture::new("install-system-default");
    f.release("1.2.3", &["linux-x86_64"]);
    f.latest(r#"{"version": "1.2.3"}"#);
    // `mkdir` and `install` are fakes that only log, and `sudo` runs its command through them, so whether or not
    // this user can write /usr/local/bin, the host's directory is not touched.
    let log = f.path("calls.log");
    f.fake("mkdir", &format!("echo \"mkdir $*\" >> {}", log.display()));
    f.fake(
        "install",
        &format!("echo \"install $*\" >> {}", log.display()),
    );
    f.fake("sudo", "exec \"$@\"");
    let r = f.run(&["--system"]);
    assert!(r.ok(), "{}", r.stderr());
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("mkdir -p /usr/local/bin\n"), "{calls}");
    assert!(
        calls.contains("/tierstore /usr/local/bin/tierstore\n"),
        "{calls}"
    );
}

#[test]
fn unknown_flags_and_missing_values_are_usage_errors_and_help_prints_usage() {
    let f = Fixture::new("install-usage");
    for (args, why) in [
        (vec!["--frobnicate"], "unknown argument --frobnicate"),
        (vec!["--prefix"], "--prefix needs a value"),
        (vec!["--version"], "--version needs a value"),
    ] {
        let r = f.run(&args);
        assert!(!r.ok(), "{args:?}");
        assert!(r.stderr().contains(why), "{args:?}: {}", r.stderr());
    }
    assert!(f.requested().is_empty());
    for flag in ["--help", "-h"] {
        let r = f.run(&[flag]);
        assert!(r.ok());
        assert!(
            r.stdout().starts_with("usage: install.sh"),
            "{}",
            r.stdout()
        );
    }
}
