use std::cell::RefCell;

use super::*;
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const SAYS: &str = "tierstore 1.2.3+0123456789ab\n";
const URL: &str = "https://packages.komira.ai/apt";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside.parent().expect("deps").join("xtask-check-unit"),
        name,
    )
}

/// A machine that answers for `apt-get`, `dpkg`, `dpkg-query`, `curl`, `brew`, `sh` and the
/// installed binary, and does to a scratch root what each would do to a real one.
struct Machine {
    root: PathBuf,
    calls: RefCell<Vec<String>>,
    arch: &'static str,
    keyring: Vec<u8>,
    dpkg_version: &'static str,
    says: &'static str,
    /// Files below the root that the install does not create.
    omit: Vec<&'static str>,
    /// Links below `etc/systemd/system` the install creates.
    enable: Vec<&'static str>,
    /// The mode the install gives `etc/tierstore`; none: it makes no directory there.
    config_dir: Option<u32>,
    /// Files below the root the install writes that the package must not.
    stray: Vec<&'static str>,
    /// Does a process named `tierstore` run?
    running: bool,
    /// A command line (joined) containing this text is refused.
    refuse: Option<&'static str>,
}

impl Machine {
    fn new(root: &Path) -> Machine {
        Machine {
            root: root.to_path_buf(),
            calls: RefCell::new(Vec::new()),
            arch: "amd64",
            keyring: vec![0x99, 0x01, 0x0d],
            dpkg_version: "1.2.3",
            says: SAYS,
            omit: Vec::new(),
            enable: Vec::new(),
            config_dir: Some(0o750),
            stray: Vec::new(),
            running: false,
            refuse: None,
        }
    }

    /// The refusal for the command line `line`, when the machine is told to refuse it.
    fn refusal(&self, line: &str) -> Option<Answer> {
        let text = self.refuse.filter(|text| line.contains(text))?;
        Some(Answer {
            ok: false,
            stdout: String::new(),
            stderr: format!("refused: {text}\n"),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        write(&self.root.join(rel), bytes).expect("write");
    }

    /// The image's dpkg drop-in of a minimized Ubuntu, as the job's `ubuntu:24.04` has it.
    fn minimized(&self) {
        self.put(
            DPKG_EXCLUDES,
            b"path-exclude=/usr/share/man/*\npath-exclude=/usr/share/doc/*\npath-include=/usr/share/doc/*/copyright\n",
        );
    }

    /// Is `rel` left out by a `path-exclude=<dir>/*` line of the dpkg drop-ins below the root, as
    /// dpkg leaves it out when it unpacks a package?
    fn excluded(&self, rel: &str) -> bool {
        let dir = self.root.join("etc/dpkg/dpkg.cfg.d");
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|f| std::fs::read_to_string(f.path()).ok())
            .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
            .filter_map(|l| l.strip_prefix("path-exclude=/").map(str::to_owned))
            .any(|pattern| rel.starts_with(pattern.trim_end_matches('*')))
    }

    fn install(&self) {
        for rel in [BINARY, UNIT, SYSUSERS, EXAMPLE] {
            if !self.omit.contains(&rel) && !self.excluded(rel) {
                self.put(rel, b"file");
            }
        }
        if let Some(mode) = self.config_dir {
            use std::os::unix::fs::PermissionsExt;
            let dir = self.root.join(CONFIG_DIR);
            std::fs::create_dir_all(&dir).expect("the directory");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("mode");
        }
        for rel in &self.stray {
            self.put(rel, b"stray");
        }
        for link in &self.enable {
            self.put(link, b"link");
        }
        if self.running {
            self.put("proc/42/comm", b"tierstore\n");
        }
        self.put("proc/43/comm", b"bash\n");
    }
}

fn answer(stdout: &str) -> Result<Answer, Error> {
    Ok(Answer {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

impl Host for Machine {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let name = program
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        let line = format!("{name} {}", args.join(" "));
        self.calls.borrow_mut().push(line.clone());
        if let Some(refused) = self.refusal(&line) {
            return Ok(refused);
        }
        match (name.as_str(), args.first().map(String::as_str)) {
            ("dpkg", _) => answer(&format!("{}\n", self.arch)),
            ("dpkg-query", _) => answer(self.dpkg_version),
            ("apt-get", Some("install")) if line.contains("tierstore=") => {
                self.install();
                answer("")
            }
            ("curl", _) => {
                let at = args.iter().position(|a| a == "--output").expect("--output");
                write(Path::new(&args[at + 1]), &self.keyring).expect("keyring");
                answer("")
            }
            ("tierstore", Some("--version")) => answer(self.says),
            _ => answer(""),
        }
    }
}

fn apt<'a>(root: &'a Path) -> Apt<'a> {
    Apt {
        root,
        arch: "amd64",
        version: "1.2.3",
        commit: COMMIT,
        apt_url: URL,
        apt_get: Path::new("apt-get"),
        dpkg_query: Path::new("dpkg-query"),
        dpkg: Path::new("dpkg"),
        curl: Path::new("curl"),
    }
}

#[test]
fn a_clean_install_from_testing_is_checked_and_adds_the_repository_the_way_a_user_does() {
    // Catches: a source that is not deb822 with Signed-By and the suite testing, a package
    // installed at no version, and the order (update, upgrade, certificates, keyring, source, update, install).
    let dir = scratch("apt-ok");
    let machine = Machine::new(&dir);
    machine.minimized();
    let said = check_apt(&machine, &apt(&dir)).expect("checked");
    assert_eq!(
        machine
            .calls()
            .iter()
            .map(|c| c.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>(),
        [
            "dpkg",
            "apt-get",
            "apt-get",
            "apt-get",
            "curl",
            "apt-get",
            "apt-get",
            "dpkg-query",
            "tierstore"
        ]
    );
    assert!(
        machine
            .calls()
            .contains(&"apt-get install -y --no-install-recommends tierstore=1.2.3".to_owned())
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(SOURCES)).expect("source"),
        "Types: deb\nURIs: https://packages.komira.ai/apt\nSuites: testing\nComponents: main\nArchitectures: amd64\nSigned-By: /usr/share/keyrings/komira-archive-keyring.gpg\n"
    );
    assert!(
        machine
            .calls()
            .iter()
            .any(|c| c.ends_with("https://packages.komira.ai/apt/komira-archive-keyring.gpg"))
    );
    assert!(
        said[0]
            .starts_with("check-apt: tierstore 1.2.3 installed from the suite testing on amd64;"),
        "{said:?}"
    );
}

#[test]
fn a_package_missing_its_unit_template_sysusers_entry_or_example_is_named() {
    // Catches: a package that installs without the files the service needs.
    let dir = scratch("apt-files");
    let mut machine = Machine::new(&dir);
    machine.omit = vec![UNIT, SYSUSERS, EXAMPLE];
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        [
            "the unit template /usr/lib/systemd/system/tierstore@.service is not installed",
            "the sysusers.d entry /usr/lib/sysusers.d/tierstore.conf is not installed",
            "the example configuration /usr/share/doc/tierstore/examples/config.toml is not installed"
        ]
    );
}

#[test]
fn an_install_that_writes_the_live_configuration_or_a_start_marker_is_named() {
    // Catches: a package that writes /etc/tierstore/config.toml (the example as the live file, so
    // an instance would serve the example's tiers) or a start marker (an instance would start at
    // the next boot with no configuration of the host's).
    let dir = scratch("apt-stray");
    let mut machine = Machine::new(&dir);
    machine.stray = vec![CONFIG, "var/lib/tierstore/run-a"];
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        [
            "/etc/tierstore/config.toml is there, and the package never writes the host's configuration",
            "/var/lib/tierstore/run-a is there, and the package writes no start marker",
        ]
    );
}

#[test]
fn a_config_directory_that_is_missing_open_or_not_a_directory_is_named() {
    // Catches: a postinst that does not make /etc/tierstore, makes it readable by others, or
    // leaves something else at that path.
    for (mode, problem) in [
        (None, "/etc/tierstore was not made"),
        (Some(0o755), "/etc/tierstore has mode 0755, not 0750"),
    ] {
        let dir = scratch("apt-config-dir");
        let mut machine = Machine::new(&dir);
        machine.config_dir = mode;
        let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
            panic!("not refused");
        };
        assert_eq!(problems, [problem]);
    }
    let dir = scratch("apt-config-file");
    let mut machine = Machine::new(&dir);
    machine.config_dir = None;
    machine.stray = vec![CONFIG_DIR];
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(problems, ["/etc/tierstore is not a directory"]);
}

#[test]
fn a_service_that_the_install_enabled_or_started_is_named() {
    // Catches: a postinst that enables or starts the service on install, which the package
    // promises it does not.
    let dir = scratch("apt-enabled");
    let mut machine = Machine::new(&dir);
    machine.enable = vec![
        "etc/systemd/system/multi-user.target.wants/tierstore@a.service",
        "etc/systemd/system/multi-user.target.wants/tierstore@b.service",
        "etc/systemd/system/tierstore@.service",
    ];
    machine.running = true;
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    let enabled = |unit: &str| {
        format!(
            "the service is enabled: {} exists, and the package enables nothing",
            dir.join("etc/systemd/system").join(unit).display()
        )
    };
    assert_eq!(
        problems,
        [
            enabled("multi-user.target.wants/tierstore@a.service"),
            enabled("multi-user.target.wants/tierstore@b.service"),
            enabled("tierstore@.service"),
            "the service is running (process 42), and the package starts nothing".to_owned(),
        ]
    );
}

#[test]
fn a_package_at_another_version_or_a_binary_that_says_another_is_refused() {
    // Catches: apt installing a version other than the one asked, and a binary of another build.
    let dir = scratch("apt-version");
    let mut machine = Machine::new(&dir);
    machine.dpkg_version = "1.2.2";
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        ["dpkg says tierstore \"1.2.2\" is installed, not \"1.2.3\""]
    );
    let dir = scratch("apt-says");
    let mut machine = Machine::new(&dir);
    machine.says = "tierstore 1.2.3+ffffffffffff\n";
    let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
    assert!(e.contains("the binary says \"tierstore 1.2.3+ffffffffffff\\n\", not \"tierstore 1.2.3+0123456789ab\""), "{e}");
}

#[test]
fn an_armored_or_empty_keyring_is_refused_because_signed_by_takes_a_gpg_file() {
    // Catches: a keyring published as armor, which apt silently does not read from Signed-By.
    for keyring in [b"-----BEGIN PGP PUBLIC KEY BLOCK-----".to_vec(), Vec::new()] {
        let dir = scratch("apt-keyring");
        let mut machine = Machine::new(&dir);
        machine.keyring = keyring;
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        assert!(
            e.contains("komira-archive-keyring.gpg is not a binary keyring"),
            "{e}"
        );
        assert!(!dir.join(SOURCES).exists());
    }
}

#[test]
fn a_program_that_fails_is_named_with_what_it_said_and_a_machine_of_another_architecture_is_refused()
 {
    // Catches: an apt-get update that fails (a signature apt could not verify) read as a pass.
    for (refuse, program) in [
        ("update", "apt-get update"),
        ("dist-upgrade", "apt-get dist-upgrade"),
        ("ca-certificates", "ca-certificates"),
        ("--output", "--output"),
        ("tierstore=", "tierstore=1.2.3"),
        ("--showformat", "dpkg-query"),
        ("--version", "tierstore --version"),
        ("--print-architecture", "dpkg --print-architecture"),
    ] {
        let dir = scratch("apt-refused");
        let mut machine = Machine::new(&dir);
        machine.refuse = Some(refuse);
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        assert!(
            e.contains(program) && e.contains(&format!("failed: refused: {refuse}")),
            "{refuse}: {e}"
        );
    }
    let dir = scratch("apt-arch");
    let mut machine = Machine::new(&dir);
    machine.arch = "arm64";
    let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
    assert_eq!(
        e,
        "this machine is arm64, not amd64: the check would install another package"
    );
}

#[test]
fn flags_out_of_their_form_are_usage_errors_before_anything_runs() {
    // Catches: a check that installs from a plain-HTTP repository or for an architecture it is
    // not on.
    let dir = scratch("apt-usage");
    let machine = Machine::new(&dir);
    for change in [
        |r: &mut Apt| r.version = "v1.2.3",
        |r: &mut Apt| r.arch = "i386",
        |r: &mut Apt| r.apt_url = "http://packages.komira.ai/apt",
        |r: &mut Apt| r.apt_url = "https://packages.komira.ai/apt/",
    ] {
        let mut r = apt(&dir);
        change(&mut r);
        assert!(matches!(check_apt(&machine, &r), Err(Error::Usage(_))));
    }
    assert!(machine.calls().is_empty());
}

fn brew<'a>(formula: &'a Path, tap: &'a str) -> Brew<'a> {
    Brew {
        formula,
        version: "1.2.3",
        commit: COMMIT,
        tap,
        brew: Path::new("brew"),
    }
}

/// `brew` for a scratch prefix and tap repository.
struct Brewery {
    prefix: PathBuf,
    tap: PathBuf,
    inner: Machine,
}

impl Host for Brewery {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let name = program
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        if name == "brew" {
            let line = format!("brew {}", args.join(" "));
            self.inner.calls.borrow_mut().push(line.clone());
            if let Some(refused) = self.inner.refusal(&line) {
                return Ok(refused);
            }
            return match args[0].as_str() {
                "--repository" => answer(&format!("{}\n", self.tap.display())),
                "--prefix" => answer(&format!("{}\n", self.prefix.display())),
                "install" => {
                    write(&self.prefix.join("bin/tierstore"), b"binary").expect("binary");
                    answer("")
                }
                _ => answer(""),
            };
        }
        self.inner.run(program, args)
    }
}

#[test]
fn the_rendered_formula_is_installed_from_a_local_tap_and_the_binary_says_the_release() {
    // Catches: a formula installed from a bare path (brew refuses), not the file that was
    // rendered, and a binary of another build.
    let dir = scratch("brew-ok");
    let formula = dir.join("tierstore.rb");
    write(&formula, b"class Tierstore < Formula\nend\n").expect("formula");
    let host = Brewery {
        prefix: dir.join("prefix"),
        tap: dir.join("tap"),
        inner: Machine::new(&dir),
    };
    let said = check_brew(&host, &brew(&formula, "local/tierstore")).expect("checked");
    assert_eq!(
        host.inner.calls(),
        [
            "brew tap-new --no-git local/tierstore",
            "brew --repository local/tierstore",
            "brew install --formula local/tierstore/tierstore",
            "brew --prefix",
            "tierstore --version"
        ]
    );
    assert_eq!(
        std::fs::read(dir.join("tap/Formula/tierstore.rb")).expect("copied"),
        std::fs::read(&formula).expect("formula")
    );
    assert_eq!(
        said,
        [
            "check-brew: local/tierstore/tierstore installed from the rendered formula; it says \"tierstore 1.2.3+0123456789ab\""
        ]
    );
    let mut other = Machine::new(&dir);
    other.says = "tierstore 1.2.2+0123456789ab\n";
    let host = Brewery {
        prefix: dir.join("prefix"),
        tap: dir.join("tap"),
        inner: other,
    };
    assert!(check_brew(&host, &brew(&formula, "local/tierstore")).is_err());
}

#[test]
fn a_formula_that_is_missing_a_failed_brew_and_flags_out_of_their_form_are_errors() {
    // Catches: a missing formula read as an install, and a tap that is not owner/name.
    let dir = scratch("brew-errors");
    let host = Brewery {
        prefix: dir.join("prefix"),
        tap: dir.join("tap"),
        inner: Machine::new(&dir),
    };
    let missing = dir.join("missing.rb");
    let e = check_brew(&host, &brew(&missing, "local/tierstore"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("missing.rb"), "{e}");
    let formula = dir.join("tierstore.rb");
    write(&formula, b"formula").expect("formula");
    for tap in ["tierstore", "a/b/c", "/b", "a/"] {
        assert!(
            matches!(
                check_brew(&host, &brew(&formula, tap)),
                Err(Error::Usage(_))
            ),
            "{tap}"
        );
    }
    let mut r = brew(&formula, "local/tierstore");
    r.version = "v1";
    assert!(matches!(check_brew(&host, &r), Err(Error::Usage(_))));
    let mut failing = Machine::new(&dir);
    failing.refuse = Some("tap-new");
    let host = Brewery {
        prefix: dir.join("prefix"),
        tap: dir.join("tap"),
        inner: failing,
    };
    let e = check_brew(&host, &brew(&formula, "local/tierstore"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("brew tap-new --no-git local/tierstore failed"),
        "{e}"
    );
}

fn install_sh<'a>(work: &'a Path) -> InstallSh<'a> {
    InstallSh {
        version: "1.2.3",
        commit: COMMIT,
        url: "https://tierstore.dev",
        work,
        curl: Path::new("curl"),
        sh: Path::new("sh"),
    }
}

/// `sh` that installs a binary where `--prefix` says.
struct Shell(Machine);

impl Host for Shell {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        if program.file_name().is_some_and(|n| n == "sh") {
            let line = format!("sh {}", args.join(" "));
            self.0.calls.borrow_mut().push(line.clone());
            if let Some(refused) = self.0.refusal(&line) {
                return Ok(refused);
            }
            let at = args.iter().position(|a| a == "--prefix").expect("--prefix");
            write(&Path::new(&args[at + 1]).join("tierstore"), b"binary").expect("binary");
            return answer("");
        }
        self.0.run(program, args)
    }
}

#[test]
fn install_sh_from_the_site_installs_the_version_into_a_scratch_prefix_and_the_binary_says_it() {
    // Catches: a script run from the repository instead of the site, --version not passed (the
    // newest installed instead), and a binary of another build.
    let dir = scratch("sh-ok");
    let host = Shell(Machine::new(&dir));
    let said = check_install_sh(&host, &install_sh(&dir)).expect("checked");
    let calls = host.0.calls();
    assert!(
        calls[0].starts_with(
            "curl --proto =https --tlsv1.2 --fail --silent --show-error --create-dirs --output "
        ),
        "{calls:?}"
    );
    assert!(
        calls[0].ends_with("/install.sh https://tierstore.dev/install.sh"),
        "{calls:?}"
    );
    assert!(calls[1].starts_with("sh "), "{calls:?}");
    assert!(
        calls[1].contains("install.sh --version 1.2.3 --prefix "),
        "{calls:?}"
    );
    assert!(
        calls[1].ends_with("--base-url https://tierstore.dev"),
        "{calls:?}"
    );
    assert_eq!(calls[2], "tierstore --version");
    assert_eq!(
        said,
        [
            "check-install-sh: install.sh from https://tierstore.dev installed 1.2.3; it says \"tierstore 1.2.3+0123456789ab\""
        ]
    );
    let mut other = Machine::new(&dir);
    other.says = "tierstore 1.2.9+0123456789ab\n";
    assert!(check_install_sh(&Shell(other), &install_sh(&dir)).is_err());
}

#[test]
fn a_script_that_cannot_be_downloaded_or_fails_and_flags_out_of_their_form_are_errors() {
    // Catches: a 404 for install.sh, or a failing install, read as a pass.
    for refuse in ["--output", "--version 1.2.3"] {
        let dir = scratch("sh-refused");
        let mut machine = Machine::new(&dir);
        machine.refuse = Some(refuse);
        let e = check_install_sh(&Shell(machine), &install_sh(&dir))
            .unwrap_err()
            .to_string();
        assert!(e.contains(&format!("failed: refused: {refuse}")), "{e}");
    }
    let dir = scratch("sh-usage");
    let host = Shell(Machine::new(&dir));
    for change in [
        |r: &mut InstallSh| r.version = "1.2",
        |r: &mut InstallSh| r.url = "http://tierstore.dev",
        |r: &mut InstallSh| r.url = "https://tierstore.dev/",
    ] {
        let mut r = install_sh(&dir);
        change(&mut r);
        assert!(matches!(check_install_sh(&host, &r), Err(Error::Usage(_))));
    }
    assert!(host.0.calls().is_empty());
}

#[test]
fn the_image_is_updated_and_upgraded_before_certificates_and_curl_are_installed() {
    // Catches: the failure of the first release's amd64 install check, `apt-get install -y
    // ca-certificates curl` on the digest-pinned ubuntu:24.04 image refused with "you have held
    // broken packages" because the image's packages are older than the archive's: the apt-get
    // calls are `update`, then the upgrade, then the install of the two packages, before the
    // repository is added, and the second `update` follows the source file.
    let dir = scratch("apt-upgrade-order");
    let machine = Machine::new(&dir);
    check_apt(&machine, &apt(&dir)).expect("checked");
    let calls: Vec<String> = machine
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("apt-get "))
        .collect();
    assert_eq!(
        calls,
        [
            "apt-get update",
            "apt-get dist-upgrade -y -o Dpkg::Options::=--force-confold",
            "apt-get install -y --no-install-recommends ca-certificates curl",
            "apt-get update",
            "apt-get install -y --no-install-recommends tierstore=1.2.3",
        ]
    );
}

#[test]
fn the_minimized_images_dpkg_drop_in_is_removed_before_the_package_is_installed_so_its_example_is_unpacked()
 {
    // Catches: the failure of the install checks of v0.3.99, "the example configuration
    // /usr/share/doc/tierstore/examples/config.toml is not installed" on both architectures,
    // because the job's ubuntu:24.04 image is minimized and its dpkg drop-in leaves out
    // everything below /usr/share/doc; and a removal made after the install, when dpkg has
    // already left the file out.
    let dir = scratch("apt-minimized");
    let machine = Machine::new(&dir);
    machine.minimized();
    assert!(machine.excluded(EXAMPLE) && !machine.excluded(UNIT));
    check_apt(&machine, &apt(&dir)).expect("checked");
    assert!(dir.join(EXAMPLE).is_file());
    assert!(!dir.join(DPKG_EXCLUDES).exists());
    // An image without the drop-in (a standard install) is checked the same way.
    let dir = scratch("apt-standard");
    let machine = Machine::new(&dir);
    check_apt(&machine, &apt(&dir)).expect("checked");
    assert!(dir.join(EXAMPLE).is_file());
}

#[test]
fn a_dpkg_drop_in_that_cannot_be_removed_fails_the_check_before_the_package_is_installed() {
    // Catches: a removal whose failure is passed over, so that the install goes on and the check
    // names a missing example instead of the drop-in it could not remove.
    let dir = scratch("apt-excludes-dir");
    let machine = Machine::new(&dir);
    std::fs::create_dir_all(dir.join(DPKG_EXCLUDES).join("x")).expect("a directory");
    let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
    assert!(
        e.contains(&format!("remove {}", dir.join(DPKG_EXCLUDES).display())),
        "{e}"
    );
    assert!(
        !machine
            .calls()
            .iter()
            .any(|c| c.contains("tierstore=1.2.3")),
        "{:?}",
        machine.calls()
    );
}
