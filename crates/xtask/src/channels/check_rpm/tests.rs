use std::cell::RefCell;

use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const URL: &str = "https://packages.komira.ai";

/// A container that answers for the programs the check runs.
struct Machine {
    calls: RefCell<Vec<String>>,
    arch: &'static str,
    installed: &'static str,
    checked: &'static str,
    says: &'static str,
    conffiles: &'static str,
    enabled: &'static str,
    /// Files the install does not create.
    omit: Vec<&'static str>,
    /// A command line (joined) containing this text is refused.
    refuse: Option<&'static str>,
}

impl Machine {
    fn new() -> Machine {
        Machine {
            calls: RefCell::new(Vec::new()),
            arch: "x86_64",
            installed: "1.2.3-1.x86_64",
            checked: "/root/tierstore-1.2.3-1.x86_64.rpm: digests signatures OK\n",
            says: "tierstore 1.2.3+0123456789ab\n",
            conffiles: "/etc/tierstore/tierstore.toml\n",
            enabled: "",
            omit: Vec::new(),
            refuse: None,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Host for Machine {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let line = format!("{} {}", program.display(), args.join(" "));
        self.calls.borrow_mut().push(line.clone());
        let ok = |stdout: &str| Answer {
            ok: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        };
        if let Some(text) = self.refuse.filter(|t| line.contains(t)) {
            return Ok(Answer {
                ok: false,
                stdout: String::new(),
                stderr: format!("refused: {}\n", text.trim()),
            });
        }
        let missing = |path: &str| self.omit.contains(&path.trim_start_matches('/'));
        Ok(match program.to_str().expect("utf8") {
            "uname" => ok(&format!("{}\n", self.arch)),
            "rpm" if args[0] == "-q" => ok(self.installed),
            "rpm" if args[0] == "-K" => ok(self.checked),
            "rpm" if args[0] == "-qc" => ok(self.conffiles),
            "sh" if args[1].starts_with("test -f") && missing(&args[3]) => Answer {
                ok: false,
                stdout: String::new(),
                stderr: String::new(),
            },
            "sh" if args[1].starts_with("ls ") => ok(self.enabled),
            "/usr/libexec/tierstore/tierstore" => ok(self.says),
            _ => ok(""),
        })
    }
}

fn ask<'a>(arch: &'a str, version: &'a str, url: &'a str, work: &'a str) -> Rpm<'a> {
    Rpm {
        arch,
        version,
        commit: COMMIT,
        url,
        work,
    }
}

#[test]
fn the_release_is_installed_from_the_repository_file_of_testing_and_held_to_what_it_promises() {
    // Catches: a check that installs from a repository with a signature check off, that does not
    // install the version it was asked for, or that never looks at the signature of the package.
    let m = Machine::new();
    let said = check_rpm(&m, &ask("x86_64", "1.2.3", URL, "/root")).expect("check");
    assert_eq!(said.len(), 1);
    assert!(
        said[0].contains("rpm -K says OK") && said[0].contains("tierstore 1.2.3+0123456789ab"),
        "{said:?}"
    );
    let calls = m.calls();
    assert_eq!(calls[0], "uname -m");
    assert!(
        calls[1].starts_with("sh -c printf '%s' \"$1\" > \"$2\" sh [komira-testing]"),
        "{}",
        calls[1]
    );
    assert!(calls[1].contains("baseurl=https://packages.komira.ai/rpm/testing/$basearch\nenabled=1\ngpgcheck=1\nrepo_gpgcheck=1\ngpgkey=https://packages.komira.ai/apt/komira-archive-keyring.asc\n"));
    assert!(calls[1].ends_with(" /etc/yum.repos.d/komira-testing.repo"));
    assert_eq!(calls[2], "dnf install -y tierstore-1.2.3");
    assert!(calls.contains(
        &"rpm --import https://packages.komira.ai/apt/komira-archive-keyring.asc".to_owned()
    ));
    assert!(calls.iter().any(|c| c.starts_with("curl ") && c.ends_with("--output /root/tierstore-1.2.3-1.x86_64.rpm https://packages.komira.ai/rpm/pool/tierstore/tierstore-1.2.3-1.x86_64.rpm")));
    assert!(calls.contains(&"rpm -K /root/tierstore-1.2.3-1.x86_64.rpm".to_owned()));
    assert!(calls.contains(&"/usr/libexec/tierstore/tierstore --version".to_owned()));
    for file in [
        "/usr/lib/systemd/system/tierstore.service",
        "/usr/lib/sysusers.d/tierstore.conf",
        "/etc/tierstore/tierstore.toml",
    ] {
        assert!(
            calls.contains(&format!("sh -c test -f \"$1\" sh {file}")),
            "{file}"
        );
    }
    assert_eq!(repo_file(URL).matches("gpgcheck=1").count(), 2);
    assert_eq!(
        file_name("1.2.3", "aarch64"),
        "tierstore-1.2.3-1.aarch64.rpm"
    );
}

#[test]
fn a_flag_out_of_its_form_or_a_machine_of_another_architecture_is_refused_before_anything_is_installed()
 {
    // Catches: the package of one architecture checked on a machine of the other, and flags that
    // turn the check into another one.
    let m = Machine::new();
    for (arch, version, url, work) in [
        ("amd64", "1.2.3", URL, "/root"),
        ("x86_64", "v1.2.3", URL, "/root"),
        ("x86_64", "1.2.3", "http://packages.komira.ai", "/root"),
        ("x86_64", "1.2.3", "https://packages.komira.ai/", "/root"),
        ("x86_64", "1.2.3", URL, "root"),
        ("x86_64", "1.2.3", URL, "/root/"),
    ] {
        assert!(
            matches!(
                check_rpm(&m, &ask(arch, version, url, work)),
                Err(Error::Usage(_))
            ),
            "{arch} {version} {url} {work}"
        );
    }
    assert!(m.calls().is_empty());
    let mut other = Machine::new();
    other.arch = "aarch64";
    let e = check_rpm(&other, &ask("x86_64", "1.2.3", URL, "/root"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("this machine is aarch64, not x86_64"), "{e}");
    assert_eq!(other.calls(), ["uname -m"]);
}

#[test]
fn a_program_that_fails_ends_the_check_with_what_it_said() {
    // Catches: a failed install, key import or download taken for a pass.
    for refusal in [
        "dnf install",
        "printf",
        "--import",
        "curl ",
        "-K ",
        "--version",
        "-qc",
        "ls /etc/systemd",
        "-q --qf",
        "uname",
    ] {
        let mut m = Machine::new();
        m.refuse = Some(refusal);
        let e = check_rpm(&m, &ask("x86_64", "1.2.3", URL, "/root"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(&format!("refused: {}", refusal.trim())),
            "{refusal}: {e}"
        );
    }
}

#[test]
fn each_way_the_installed_system_is_not_what_the_package_promises_is_a_problem() {
    // Catches: a package at another version or release, one rpm does not accept the signature of,
    // a unit, sysusers entry or conffile that is not installed (or not registered as a conffile),
    // and a service the package enabled.
    let mut m = Machine::new();
    m.installed = "1.2.2-1.x86_64";
    m.checked = "x: digests SIGNATURES NOT OK\n";
    m.omit = vec![
        "usr/lib/systemd/system/tierstore.service",
        "usr/lib/sysusers.d/tierstore.conf",
        "etc/tierstore/tierstore.toml",
    ];
    m.conffiles = "";
    m.enabled = "/etc/systemd/system/multi-user.target.wants/tierstore.service\n";
    let Err(Error::Refused(problems)) = check_rpm(&m, &ask("x86_64", "1.2.3", URL, "/root")) else {
        panic!("refused")
    };
    assert_eq!(problems.len(), 7, "{problems:?}");
    assert!(problems[0].contains("\"1.2.2-1.x86_64\" is installed, not \"1.2.3-1.x86_64\""));
    assert!(problems[1].contains("rpm -K says"));
    assert!(
        problems[2].contains("the unit /usr/lib/systemd/system/tierstore.service is not installed")
    );
    assert!(problems[3].contains("the sysusers.d entry"));
    assert!(problems[4].contains("the conffile /etc/tierstore/tierstore.toml is not installed"));
    assert!(problems[5].contains("not registered as a configuration file"));
    assert!(problems[6].contains("the service is enabled: /etc/systemd/system/multi-user.target.wants/tierstore.service exists"));
    // A signature line that says OK and NOT OK is not a pass either.
    let mut m = Machine::new();
    m.checked = "x: digests signatures OK, signatures NOT OK\n";
    assert!(check_rpm(&m, &ask("x86_64", "1.2.3", URL, "/root")).is_err());
}

#[test]
fn a_binary_that_does_not_say_the_release_fails_the_check() {
    // Catches: a package that installs a binary of another build.
    let mut m = Machine::new();
    m.says = "tierstore 1.2.2+0123456789ab\n";
    let e = check_rpm(&m, &ask("x86_64", "1.2.3", URL, "/root"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("1.2.3"), "{e}");
}

#[test]
fn the_container_runs_each_program_with_docker_exec_and_reports_a_docker_that_is_missing() {
    // Catches: a program run on the runner instead of in the container, and a missing docker
    // reported as a pass.
    let container = Container {
        docker: PathBuf::from("/nonexistent/docker"),
        name: "c".to_owned(),
    };
    let e = container
        .run(Path::new("rpm"), &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("/nonexistent/docker"), "{e}");
    // `true` and `false` stand in for docker: its arguments are ignored, its status is the answer.
    let yes = Container {
        docker: PathBuf::from("true"),
        name: "c".to_owned(),
    };
    assert!(
        yes.run(Path::new("rpm"), &["-q".to_owned()])
            .expect("run")
            .ok
    );
    let no = Container {
        docker: PathBuf::from("false"),
        name: "c".to_owned(),
    };
    assert!(!no.run(Path::new("rpm"), &[]).expect("run").ok);
}
