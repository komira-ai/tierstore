//! The pkg's `postinstall` run as the installer runs it, by `sh` with the installer's four
//! arguments, on a scratch target volume, with stand-ins of `dscl` (a directory of users and
//! groups kept in files), `chown` (which needs root) and `launchctl` first on `PATH`; each records
//! its calls. `mkdir`, `chmod`, `awk` and the shell are the system's.

use super::*;

/// `dscl`'s stand-in: it records itself; `-read` of the account's group or user answers the id
/// in the file `gid` or `uid` of `$STUB_DIR` (and fails as for a missing record when there is
/// none), `-list` prints the file `users` or `groups`, `-create` of the group's or the user's id
/// writes it to `gid` or `uid`, and any `-create` fails while the file `fail-create` exists.
const STUB_DSCL: &str = r#"#!/bin/sh
printf '%s\n' "dscl $*" >> "$STUB_LOG"
if [ "$2" = -create ] && [ -e "$STUB_DIR/fail-create" ]; then
    exit 1
fi
case "$2 $3 $4" in
"-read /Groups/_tierstore PrimaryGroupID")
    [ -s "$STUB_DIR/gid" ] || exit 56
    printf 'PrimaryGroupID: %s\n' "$(cat "$STUB_DIR/gid")" ;;
"-read /Users/_tierstore UniqueID")
    [ -s "$STUB_DIR/uid" ] || exit 56
    printf 'UniqueID: %s\n' "$(cat "$STUB_DIR/uid")" ;;
"-list /Users UniqueID") cat "$STUB_DIR/users" ;;
"-list /Groups PrimaryGroupID") cat "$STUB_DIR/groups" ;;
"-create /Groups/_tierstore PrimaryGroupID") printf '%s\n' "$5" > "$STUB_DIR/gid" ;;
"-create /Users/_tierstore UniqueID") printf '%s\n' "$5" > "$STUB_DIR/uid" ;;
"-create "*) ;;
*) exit 2 ;;
esac
"#;

/// The stand-in of `chown` and `launchctl`: it records itself.
const STUB: &str = "#!/bin/sh\nprintf '%s\\n' \"${0##*/} $*\" >> \"$STUB_LOG\"\n";

/// The users and the groups of a stock Mac near the range the script takes ids from: the
/// highest free id is 397 (398 is a user's, 399 a group's).
const USERS: &str = "root 0\n_mds_stores 308\n_spare 398\n_oahd 441\nnobody -2\n";
const GROUPS: &str = "wheel 0\ncom.apple.access_ssh 399\n_oahd 441\nhostkeeper-identity 501\n";

/// What one run did: whether it exited 0, the calls it made in order, and what it wrote to
/// stderr.
struct Ran {
    ok: bool,
    calls: Vec<String>,
    stderr: String,
}

/// A Mac for one run of the postinstall: its directory of users and groups, the account's ids
/// when it has them, and the target volume.
struct Mac {
    dir: TestDir,
}

impl Mac {
    fn new(name: &str, users: &str, groups: &str) -> Mac {
        let dir = scratch(name);
        let stubs = dir.join("stubs");
        std::fs::create_dir_all(&stubs).expect("stubs");
        for (program, text) in [("dscl", STUB_DSCL), ("chown", STUB), ("launchctl", STUB)] {
            let path = stubs.join(program);
            std::fs::write(&path, text).expect("a stand-in");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("a stand-in that runs");
        }
        std::fs::write(dir.join("users"), users).expect("users");
        std::fs::write(dir.join("groups"), groups).expect("groups");
        std::fs::create_dir(dir.join("volume")).expect("the target volume");
        Mac { dir }
    }

    /// The account's group, or user, is there with `id`.
    fn with(self, file: &str, id: &str) -> Mac {
        std::fs::write(self.dir.join(file), format!("{id}\n")).expect("an id");
        self
    }

    fn volume(&self) -> PathBuf {
        self.dir.join("volume")
    }

    /// `/usr/local/tierstore` on the target volume, and the directory `sub` of it.
    fn base(&self, sub: &str) -> PathBuf {
        let base = self.volume().join("usr/local/tierstore");
        if sub.is_empty() { base } else { base.join(sub) }
    }

    /// The id the stand-in recorded for the account's group or user, if any.
    fn id(&self, file: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(file))
            .ok()
            .map(|t| t.trim().to_owned())
    }

    /// Runs the postinstall as the installer does: `sh <script> <pkg> / <volume>/ /` (the target
    /// volume given with its trailing slash, as `/` is).
    fn run(&self) -> Ran {
        let script = self.dir.join("postinstall");
        std::fs::write(&script, POSTINSTALL).expect("the script");
        let log = self.dir.join("log");
        let _ = std::fs::remove_file(&log);
        let volume = format!("{}/", self.volume().display());
        let pkg = self.dir.join("tierstore.pkg").display().to_string();
        let out = std::process::Command::new("sh")
            .arg(&script)
            .args([pkg.as_str(), "/", &volume, "/"])
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("stubs").display()),
            )
            .env("STUB_LOG", &log)
            .env("STUB_DIR", &*self.dir)
            .output()
            .expect("sh");
        Ran {
            ok: out.status.success(),
            calls: std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The mode of the directory `sub` of `/usr/local/tierstore`, if it is there.
    fn mode(&self, sub: &str) -> Option<u32> {
        std::fs::symlink_metadata(self.base(sub))
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .map(|m| m.permissions().mode() & 0o7777)
    }
}

/// The two reads that find the account, which every run starts with.
const READS: [&str; 2] = [
    "dscl . -read /Groups/_tierstore PrimaryGroupID",
    "dscl . -read /Users/_tierstore UniqueID",
];

/// The two lists of the ids in use, read when part of the account is missing.
const LISTS: [&str; 2] = [
    "dscl . -list /Users UniqueID",
    "dscl . -list /Groups PrimaryGroupID",
];

/// The calls that make the group with `gid`.
fn group_calls(gid: &str) -> Vec<String> {
    let g = "dscl . -create /Groups/_tierstore";
    vec![
        g.to_owned(),
        format!("{g} RealName tierstore"),
        format!("{g} Password *"),
        format!("{g} PrimaryGroupID {gid}"),
    ]
}

/// The calls that make the user with `uid` in the group `gid`.
fn user_calls(uid: &str, gid: &str) -> Vec<String> {
    let u = "dscl . -create /Users/_tierstore";
    vec![
        u.to_owned(),
        format!("{u} RealName tierstore"),
        format!("{u} Password *"),
        format!("{u} UserShell /usr/bin/false"),
        format!("{u} NFSHomeDirectory /var/empty"),
        format!("{u} PrimaryGroupID {gid}"),
        format!("{u} UniqueID {uid}"),
    ]
}

/// The `chown` calls of the directories for the account `uid`:`gid`.
fn chown_calls(mac: &Mac, uid: &str, gid: &str) -> Vec<String> {
    let at = |sub: &str| mac.base(sub).display().to_string();
    vec![
        format!("chown 0:0 {}", at("")),
        format!("chown 0:{gid} {}", at("etc")),
        format!("chown {uid}:{gid} {}", at("cache")),
        format!("chown {uid}:{gid} {}", at("log")),
        format!("chown {uid}:{gid} {}", at("run")),
        format!("chown {uid}:{gid} {}", at("job-trees")),
        format!("chown {uid}:{gid} {}", at("put")),
    ]
}

/// Every call of a run, in order.
fn calls(parts: &[&[String]]) -> Vec<String> {
    parts.concat()
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| (*l).to_owned()).collect()
}

/// The directories of a good run, with their modes, and nothing else under the volume.
fn assert_directories(mac: &Mac) {
    assert_eq!(mac.mode(""), Some(0o755));
    assert_eq!(mac.mode("etc"), Some(0o750));
    for sub in ["cache", "log", "run"] {
        assert_eq!(mac.mode(sub), Some(0o700), "{sub}");
    }
    assert_eq!(mac.mode("job-trees"), Some(0o755));
    assert_eq!(mac.mode("put"), Some(0o755));
}

#[test]
fn on_a_fresh_mac_it_makes_the_account_with_the_highest_free_id_and_the_directories() {
    // Catches: a postinstall that leaves the job's UserName and GroupName unmade (launchd would
    // not start it), gives the user and the group other ids, takes an id a user or a group has
    // (398 and 399 here), writes an id before the record's other keys (a stopped install would
    // leave a record the next one takes as made), makes a directory with another owner or mode
    // (etc readable by others, a cache the job cannot write) or anywhere but under the target
    // volume, makes anything else there, or loads the job behind the update hook's back.
    let mac = Mac::new("postinstall-fresh", USERS, GROUPS);
    let ran = mac.run();
    assert!(ran.ok, "{}", ran.stderr);
    assert_eq!(
        ran.calls,
        calls(&[
            &strings(&READS),
            &strings(&LISTS),
            &group_calls("397"),
            &user_calls("397", "397"),
            &chown_calls(&mac, "397", "397"),
        ])
    );
    assert_eq!(ran.stderr, "");
    assert_eq!(mac.id("gid").as_deref(), Some("397"));
    assert_eq!(mac.id("uid").as_deref(), Some("397"));
    assert_directories(&mac);
    let seen: Vec<String> = tree_of(&mac.volume())
        .into_iter()
        .map(|(p, ..)| p)
        .collect();
    assert_eq!(
        seen,
        [
            "",
            "usr",
            "usr/local",
            "usr/local/tierstore",
            "usr/local/tierstore/cache",
            "usr/local/tierstore/etc",
            "usr/local/tierstore/job-trees",
            "usr/local/tierstore/log",
            "usr/local/tierstore/put",
            "usr/local/tierstore/run",
        ]
    );
}

#[test]
fn on_a_mac_with_the_account_it_keeps_it_and_sets_the_directories_again_keeping_their_files() {
    // Catches: a postinstall that makes the account again or changes its ids on an update or a
    // second install, one that leaves a directory made earlier with another mode (the etc a
    // config package laid down 0755) or owner, and one that changes, removes or chmods the
    // configuration, a key file or the cache in them.
    let mac = Mac::new("postinstall-there", USERS, GROUPS)
        .with("gid", "250")
        .with("uid", "251");
    for (sub, mode) in [("etc", 0o755), ("cache", 0o777), ("log", 0o700)] {
        std::fs::create_dir_all(mac.base(sub)).expect("a directory there");
        std::fs::set_permissions(mac.base(sub), std::fs::Permissions::from_mode(mode))
            .expect("mode");
    }
    let config = mac.base("etc").join("tierstore.toml");
    std::fs::write(&config, b"# the Mac's\n").expect("the configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let pack = mac.base("cache").join("pack");
    std::fs::write(&pack, b"cached").expect("a cached file");
    std::fs::set_permissions(&pack, std::fs::Permissions::from_mode(0o600)).expect("mode");
    for round in 0..2 {
        let ran = mac.run();
        assert!(ran.ok, "{}", ran.stderr);
        assert_eq!(
            ran.calls,
            calls(&[&strings(&READS), &chown_calls(&mac, "251", "250")]),
            "round {round}"
        );
        assert_eq!(mac.id("gid").as_deref(), Some("250"));
        assert_eq!(mac.id("uid").as_deref(), Some("251"));
        assert_directories(&mac);
        assert_eq!(std::fs::read(&config).expect("kept"), b"# the Mac's\n");
        assert_eq!(std::fs::read(&pack).expect("kept"), b"cached");
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o7777;
        assert_eq!((mode(&config), mode(&pack)), (0o644, 0o600));
    }
}

#[test]
fn with_half_the_account_it_makes_the_other_half_with_a_free_id_and_the_group_it_has() {
    // Catches: a postinstall that makes a missing user in another group than the one there,
    // gives it the group's id when a user already has that id, makes again or renumbers the
    // half that is there, or takes an id the group or the user there has.
    let groups = format!("{GROUPS}_tierstore 397\n");
    let mac = Mac::new("postinstall-group-there", USERS, &groups).with("gid", "397");
    let ran = mac.run();
    assert!(ran.ok, "{}", ran.stderr);
    assert_eq!(
        ran.calls,
        calls(&[
            &strings(&READS),
            &strings(&LISTS),
            &user_calls("396", "397"),
            &chown_calls(&mac, "396", "397"),
        ])
    );
    assert_directories(&mac);
    let users = format!("{USERS}_tierstore 397\n");
    let mac = Mac::new("postinstall-user-there", &users, GROUPS).with("uid", "397");
    let ran = mac.run();
    assert!(ran.ok, "{}", ran.stderr);
    assert_eq!(
        ran.calls,
        calls(&[
            &strings(&READS),
            &strings(&LISTS),
            &group_calls("396"),
            &chown_calls(&mac, "397", "396"),
        ])
    );
    assert_directories(&mac);
}

#[test]
fn with_no_free_id_from_399_to_300_it_fails_and_makes_nothing() {
    // Catches: a postinstall that takes an id in use (or one outside the range, among the
    // people's from 501 or below the system's own) when every id of the range is taken, and one
    // that makes half an account or the directories before it fails.
    let users: String = (300..=399).map(|id| format!("_u{id} {id}\n")).collect();
    let mac = Mac::new("postinstall-no-id", &users, GROUPS);
    let ran = mac.run();
    assert!(!ran.ok);
    assert_eq!(ran.calls, calls(&[&strings(&READS), &strings(&LISTS)]));
    assert_eq!(
        ran.stderr,
        "tierstore postinstall: no id from 399 down to 300 is free for _tierstore\n"
    );
    assert_eq!((mac.id("gid"), mac.id("uid")), (None, None));
    assert!(!mac.volume().join("usr").exists());
}

#[test]
fn a_directory_service_that_refuses_a_record_fails_the_install_before_any_directory() {
    // Catches: a postinstall that goes on past a failed dscl (no `set -e`), and so makes the
    // directories for an account that is not there and reports the install good.
    let mac = Mac::new("postinstall-dscl-fails", USERS, GROUPS);
    std::fs::write(mac.dir.join("fail-create"), b"").expect("the switch");
    let ran = mac.run();
    assert!(!ran.ok);
    assert_eq!(
        ran.calls,
        calls(&[
            &strings(&READS),
            &strings(&LISTS),
            &["dscl . -create /Groups/_tierstore".to_owned()],
        ])
    );
    assert!(!mac.volume().join("usr").exists());
}
