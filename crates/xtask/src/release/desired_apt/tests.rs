use std::cell::RefCell;
use std::io::Read;

use serde_json::{Value, json};

use super::*;
use crate::channels::apt::deb::control_of;
use crate::channels::apt::index::sha256_hex;
use crate::channels::apt::package_names;
use crate::channels::aws::Answer;
use crate::fetch::Sha256;
use crate::release::hostkeeper_package::manifest;
use crate::sums::{Line, render};
use crate::testdir::TestDir;

const REPO: &str = "example/tierstore";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const FLOOR: &str = "0.1.225";
const URL: &str = "https://packages.komira.ai/apt";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside
            .parent()
            .expect("deps")
            .join("xtask-desired-apt-unit"),
        name,
    )
}

/// The architecture a package file is for (`tierstore_<version>_<arch>.deb`).
fn arch_of(name: &str) -> &str {
    name.trim_end_matches(".deb")
        .rsplit('_')
        .next()
        .unwrap_or_default()
}

fn deb_bytes(name: &str) -> Vec<u8> {
    format!("bytes of {name}").into_bytes()
}

fn sums_text(names: &[String]) -> Vec<u8> {
    let lines: Vec<Line> = names
        .iter()
        .map(|n| Line {
            sha256: Sha256::of(&deb_bytes(n)),
            name: n.clone(),
        })
        .collect();
    render(&lines).into_bytes()
}

/// GitHub: the release v1.2.3, with assets, and the files `gh release download` writes.
struct FakeGh {
    release: Option<Value>,
    sums: Vec<u8>,
    ran: RefCell<Vec<Vec<String>>>,
}

impl Gh for FakeGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        assert_eq!(path, format!("repos/{REPO}/releases/tags/v1.2.3"));
        Ok(self.release.clone())
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.ran.borrow_mut().push(args.to_vec());
        std::fs::write(Path::new(&args[6]).join(SUMS), &self.sums).expect("write");
        Ok(String::new())
    }
}

fn release(with_sums: bool) -> Option<Value> {
    let assets: Vec<Value> = if with_sums {
        vec![json!({"name": SUMS, "size": 10})]
    } else {
        Vec::new()
    };
    Some(json!({"draft": false, "prerelease": false, "target_commitish": COMMIT, "assets": assets}))
}

/// The testing suite as the CDN serves it: the `Packages` text per URL, or a refusal.
struct Cdn {
    packages: Vec<(String, Result<String, String>)>,
    asked: RefCell<Vec<String>>,
}

impl Host for Cdn {
    fn run(&self, _program: &Path, args: &[String]) -> Result<Answer, Error> {
        let url = args.last().expect("a url").clone();
        self.asked.borrow_mut().push(url.clone());
        let at = args.iter().position(|a| a == "--output").expect("--output");
        match self.packages.iter().find(|(u, _)| *u == url) {
            Some((_, Ok(text))) => {
                std::fs::write(&args[at + 1], text).expect("write");
                Ok(Answer {
                    ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            Some((_, Err(why))) => Ok(Answer {
                ok: false,
                stdout: String::new(),
                stderr: format!("{why}\n"),
            }),
            None => panic!("unexpected url {url}"),
        }
    }
}

/// The `Packages` text listing `name` with `digest`.
fn stanza(name: &str, digest: &str) -> String {
    let arch = arch_of(name);
    format!(
        "Package: tierstore\nVersion: 1.2.3\nArchitecture: {arch}\nFilename: pool/tierstore/{name}\nSize: 1\nSHA256: {digest}\n\n"
    )
}

fn good_cdn() -> Cdn {
    let packages = package_names("1.2.3")
        .expect("names")
        .iter()
        .map(|n| {
            (
                packages_url(URL, arch_of(n)),
                Ok(stanza(n, &sha256_hex(&deb_bytes(n)))),
            )
        })
        .collect();
    Cdn {
        packages,
        asked: RefCell::new(Vec::new()),
    }
}

fn gh() -> FakeGh {
    FakeGh {
        release: release(true),
        sums: sums_text(&package_names("1.2.3").expect("names")),
        ran: RefCell::new(Vec::new()),
    }
}

fn run(gh: &FakeGh, cdn: &Cdn, dir: &Path, output: Option<&Path>) -> Result<Vec<String>, Error> {
    ask(gh, cdn, "1.2.3", dir, URL, output)
}

fn ask(
    gh: &FakeGh,
    cdn: &Cdn,
    version: &str,
    dir: &Path,
    apt_url: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let request = Request {
        curl: Path::new("curl"),
        repo: REPO,
        version,
        min_hostkeeper: FLOOR,
        dir,
        apt_url,
    };
    desired_apt(gh, cdn, &request, output)
}

fn refused(result: Result<Vec<String>, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(problems)) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

/// The digest of the `.deb` named `name` in the fake release.
fn digest_of(name: &str) -> String {
    Sha256::of(&deb_bytes(name)).to_string()
}

/// The entry sent for one architecture, written out in full.
fn expected_entry(arch: &str) -> Value {
    json!({
        "installer": "apt",
        "repository": "komira-testing",
        "packageVersion": "1.2.3",
        "digest": digest_of(&format!("tierstore_1.2.3_{arch}.deb")),
        "manifest": {
            "package": "tierstore",
            "hook": {
                "command": ["/usr/libexec/tierstore/tierstore", "update-hook"],
                "boot": "restore",
                "timeoutSecs": {
                    "prepare": "0", "cutover": "300", "verify": "1000",
                    "rollback": "600", "cleanup": "1300", "uninstall": "300",
                },
                "verifyDeadlineSecs": "1600",
                "promotionCeilingSecs": "3600",
                "cleanupRetries": 3,
            },
            "run": ["tierstore@a.service", "tierstore@b.service"],
            "healthUrl": "http://127.0.0.1:9300/readyz",
            "params": {
                "config_file": {"text": "/etc/tierstore/config.toml"},
                "service_manager": {"text": "systemd"},
            },
            "minHostkeeper": "0.1.225",
            "installerBounds": {"stageSecs": "600", "activateSecs": "300", "removeSecs": "300"},
        },
    })
}

#[test]
fn a_release_the_testing_suite_lists_with_the_digests_of_its_sums_gives_the_package_version_and_one_apt_entry_per_architecture()
 {
    // Catches: a lab told to install a version testing does not hold, an entry that pins the
    // digest of the other architecture or not the sums' bytes, a platform key that is not the
    // host's (`linux-x86_64`, `linux-arm64`), a macOS entry (there is no package), and a
    // `package-digest` output that the digest-only form needed.
    let dir = scratch("ok");
    let out = dir.join("out");
    let cdn = good_cdn();
    let said = run(&gh(), &cdn, &dir, Some(&out)).expect("desired");
    assert_eq!(
        cdn.asked.borrow().as_slice(),
        [
            "https://packages.komira.ai/apt/dists/testing/main/binary-amd64/Packages",
            "https://packages.komira.ai/apt/dists/testing/main/binary-arm64/Packages"
        ]
    );
    let written = std::fs::read_to_string(&out).expect("output");
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines.len(), 2, "{written}");
    assert_eq!(lines[0], "package-version=1.2.3");
    let targets: Value =
        serde_json::from_str(lines[1].strip_prefix("targets=").expect("targets")).expect("JSON");
    assert_eq!(
        targets,
        json!({
            "linux-x86_64": {"entries": [expected_entry("amd64")]},
            "linux-arm64": {"entries": [expected_entry("arm64")]},
        })
    );
    assert_ne!(
        targets["linux-x86_64"]["entries"][0]["digest"],
        targets["linux-arm64"]["entries"][0]["digest"]
    );
    assert!(
        said[0].starts_with("release v1.2.3 (0123456789abcdef")
            && said[0].contains("for each of 2 architectures"),
        "{said:?}"
    );
    assert_eq!(said[1], "package-version=1.2.3");
    assert_eq!(said[2], lines[1]);
    assert!(!written.contains("package-digest"));
    // Without --output the same lines are only printed; an output that cannot be appended to
    // fails the verb, so that the job does not pass without its answer.
    assert_eq!(run(&gh(), &good_cdn(), &dir, None).expect("desired"), said);
    assert!(run(&gh(), &good_cdn(), &dir, Some(&dir)).is_err());
}

#[test]
fn the_hook_and_health_of_an_entry_are_those_of_the_archive_manifest() {
    // Catches: the entry's hook bounds, deadlines, or health URL drifting from
    // `packaging/hostkeeper/manifest.toml.in` (the archive that stays until the farm has moved),
    // so that the same node is updated under two different sets of bounds.
    let archive: toml::Table = manifest(Platform::LinuxX86_64, "1.2.3+0123456789ab", FLOOR)
        .parse()
        .expect("the archive manifest is TOML");
    let hook = archive["hook"].as_table().expect("hook");
    let sent = entry("1.2.3", &digest_of("x"), FLOOR);
    let sent_hook = &sent["manifest"]["hook"];
    for (key, camel) in [
        ("verify_deadline_secs", "verifyDeadlineSecs"),
        ("promotion_ceiling_secs", "promotionCeilingSecs"),
    ] {
        let want = hook[key].as_integer().expect(key).to_string();
        assert_eq!(sent_hook[camel], json!(want), "{key}");
    }
    assert_eq!(
        sent_hook["cleanupRetries"],
        json!(hook["cleanup_retries"].as_integer().expect("retries"))
    );
    assert_eq!(
        sent_hook["boot"],
        json!(hook["boot"].as_str().expect("boot"))
    );
    for (step, want) in hook["timeout_secs"].as_table().expect("timeouts") {
        assert_eq!(
            sent_hook["timeoutSecs"][step],
            json!(want.as_integer().expect(step).to_string()),
            "{step}"
        );
    }
    assert_eq!(
        sent["manifest"]["healthUrl"],
        json!(archive["health"]["url"].as_str().expect("health url"))
    );
}

#[test]
fn the_apt_entry_runs_the_hook_of_the_package_on_the_instances_of_its_template() {
    // Catches: an entry whose run units are not the instances the template of the `.deb` makes
    // (hostkeeper would start a unit the package does not have, or the built-in steps would
    // restart both instances where the hook hands over between them), a hook command that names
    // a program the package does not install or that is not executable (stage refuses it), a hook
    // that is not `update-hook` with the archive's leading arguments, and params that differ from
    // the archive's, so that `prepare` checks another file than the instances read, or none.
    let archive: toml::Table = manifest(Platform::LinuxX86_64, "1.2.3+0123456789ab", FLOOR)
        .parse()
        .expect("the archive manifest is TOML");
    let sent = entry("1.2.3", &digest_of("x"), FLOOR);
    let manifest = &sent["manifest"];
    let units: Vec<&str> = archive["units"]["run"]
        .as_array()
        .expect("run")
        .iter()
        .map(|u| u.as_str().expect("a unit"))
        .collect();
    assert_eq!(manifest["run"], json!(units));
    for unit in &units {
        let (template, instance) = unit.split_once('@').expect("an instance");
        assert_eq!(
            format!("usr/lib/systemd/system/{template}@.service"),
            super::super::deb::UNIT_PATH,
            "{instance}"
        );
    }
    let command: Vec<&str> = manifest["hook"]["command"]
        .as_array()
        .expect("a command")
        .iter()
        .map(|w| w.as_str().expect("a word"))
        .collect();
    let archive_command: Vec<&str> = archive["hook"]["command"]
        .as_array()
        .expect("the archive's command")
        .iter()
        .map(|w| w.as_str().expect("a word"))
        .collect();
    assert_eq!(command[1..], archive_command[1..]);
    let program = command[0].strip_prefix('/').expect("an absolute path");
    assert!(
        super::super::deb::files(b"binary", b"notice")
            .iter()
            .any(|(path, mode, _)| path == program && mode & 0o111 == 0o111),
        "{program} is not an executable file of the package"
    );
    let params = archive["params"].as_table().expect("params");
    let sent_params = manifest["params"].as_object().expect("params");
    assert_eq!(sent_params.len(), params.len());
    for (key, value) in params {
        assert_eq!(
            sent_params[key],
            json!({"text": value.as_str().expect(key)}),
            "{key}"
        );
    }
}

#[test]
fn a_package_testing_does_not_list_or_lists_with_another_digest_is_named() {
    // Catches: the lab being sent to bytes other than the ones the release was checked against.
    let dir = scratch("problems");
    let mut cdn = good_cdn();
    cdn.packages[0].1 = Ok(String::new());
    cdn.packages[1].1 = Ok(stanza("tierstore_1.2.3_arm64.deb", &sha256_hex(b"other")));
    let problems = refused(run(&gh(), &cdn, &dir, None));
    assert_eq!(
        problems[0],
        "the testing suite lists no tierstore_1.2.3_amd64.deb: the release is not published there"
    );
    assert!(
        problems[1].starts_with("the testing suite lists tierstore_1.2.3_arm64.deb with sha256 "),
        "{problems:?}"
    );
    assert_eq!(problems.len(), 2);
}

#[test]
fn a_packages_file_that_cannot_be_read_or_is_not_one_and_sums_without_a_package_are_named() {
    // Catches: a CDN failure or a half-written index read as a published release.
    let dir = scratch("unread");
    let mut cdn = good_cdn();
    cdn.packages[0].1 = Err("curl: (22) The requested URL returned error: 404".to_owned());
    let problems = refused(run(&gh(), &cdn, &dir, None));
    assert_eq!(problems.len(), 1);
    assert!(problems[0].ends_with("binary-amd64/Packages could not be read: curl: (22) The requested URL returned error: 404"), "{problems:?}");
    let mut cdn = good_cdn();
    cdn.packages[0].1 = Ok("not a stanza".to_owned());
    let e = run(&gh(), &cdn, &dir, None).unwrap_err().to_string();
    assert!(e.contains("is not a Packages file"), "{e}");
    let mut short = gh();
    short.sums = sums_text(&["tierstore_1.2.3_amd64.deb".to_owned()]);
    let problems = refused(run(&short, &good_cdn(), &dir, None));
    assert_eq!(
        problems,
        ["SHA256SUMS of release v1.2.3 lists no tierstore_1.2.3_arm64.deb"]
    );
}

#[test]
fn a_release_that_is_missing_or_has_no_sums_and_flags_out_of_their_form_are_errors() {
    // Catches: a lab set for a release that does not exist, or from a non-https repository.
    let dir = scratch("errors");
    let mut no_release = gh();
    no_release.release = None;
    assert!(run(&no_release, &good_cdn(), &dir, None).is_err());
    let mut no_sums = gh();
    no_sums.release = release(false);
    let problems = refused(run(&no_sums, &good_cdn(), &dir, None));
    assert_eq!(
        problems,
        ["release v1.2.3 has no SHA256SUMS: there are no digests to hold the testing suite to"]
    );
    for url in [
        "http://packages.komira.ai/apt",
        "https://packages.komira.ai/apt/",
    ] {
        let e = ask(&gh(), &good_cdn(), "1.2.3", &dir, url, None);
        assert!(matches!(e, Err(Error::Usage(_))), "{url}");
    }
    let e = ask(&gh(), &good_cdn(), "v1.2.3", &dir, URL, None);
    assert!(matches!(e, Err(Error::Usage(_))));
}

#[test]
fn a_hostkeeper_floor_that_is_not_a_plain_version_is_a_usage_error() {
    // Catches: a `minHostkeeper` such as `v0.1.216` or `0.1` sent on to the coordinator, which
    // refuses the whole set, after the lab stage has read the suite.
    let dir = scratch("floor");
    for floor in ["v0.1.216", "0.1", "0.1.216+abc", ""] {
        let request = Request {
            curl: Path::new("curl"),
            repo: REPO,
            version: "1.2.3",
            min_hostkeeper: floor,
            dir: &dir,
            apt_url: URL,
        };
        let e = desired_apt(&gh(), &good_cdn(), &request, None).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("--min-hostkeeper {floor:?} is not <major>.<minor>.<patch>"),
        );
    }
}

/// The systemd unit directories a package may put a unit file in (below the root).
const UNIT_DIRS: [&str; 3] = [
    "usr/lib/systemd/system/",
    "lib/systemd/system/",
    "etc/systemd/system/",
];

/// The regular files of the package `deb` (an `ar` archive whose `data.tar.gz` holds the files):
/// path below the root, without `./`, and mode.
fn data_files(deb: &[u8]) -> Vec<(String, u32)> {
    let mut rest = deb.strip_prefix(b"!<arch>\n").expect("an ar archive");
    let data = loop {
        assert!(rest.len() >= 60, "no data.tar.gz in the package");
        let name = String::from_utf8_lossy(&rest[..16]).trim_end().to_owned();
        let len: usize = String::from_utf8_lossy(&rest[48..58])
            .trim_end()
            .parse()
            .expect("a member size");
        if name == "data.tar.gz" {
            break &rest[60..60 + len];
        }
        rest = &rest[60 + len + len % 2..];
    };
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut tar)
        .expect("gzip");
    let mut files = Vec::new();
    for entry in tar::Archive::new(&tar[..]).entries().expect("a tar") {
        let entry = entry.expect("an entry");
        if entry.header().entry_type() == tar::EntryType::Regular {
            let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
            let path = path.strip_prefix("./").unwrap_or(&path).to_owned();
            files.push((path, entry.header().mode().expect("a mode")));
        }
    }
    files
}

/// Every way the run units and the hook program of the entry `sent` differ from what the package
/// whose regular files are `files` installs: a run unit must be a unit file of the package or an
/// instance (`name@i.service`) of one of its templates (`name@.service`), never a template; each
/// unit file must be run, a template by at least one instance; and the hook's program must be an
/// executable file of the package.
fn install_problems(sent: &Value, files: &[(String, u32)]) -> Vec<String> {
    let units: Vec<&str> = files
        .iter()
        .filter_map(|(path, _)| UNIT_DIRS.iter().find_map(|d| path.strip_prefix(d)))
        .collect();
    let run: Vec<&str> = sent["manifest"]["run"]
        .as_array()
        .expect("run")
        .iter()
        .map(|u| u.as_str().expect("a unit"))
        .collect();
    let template_of = |unit: &str| {
        let (name, rest) = unit.split_once('@')?;
        let instance = rest.strip_suffix(".service")?;
        (!instance.is_empty()).then(|| format!("{name}@.service"))
    };
    let mut problems = Vec::new();
    for unit in &run {
        if unit.ends_with("@.service") {
            problems.push(format!(
                "the entry runs the template {unit}, not an instance of it"
            ));
        } else if !units.contains(unit)
            && !template_of(unit).is_some_and(|t| units.contains(&t.as_str()))
        {
            problems.push(format!(
                "the entry runs {unit}, which is neither a unit file of the package nor an instance of one of its templates"
            ));
        }
    }
    for unit in &units {
        let ran = if unit.ends_with("@.service") {
            run.iter().any(|r| template_of(r).as_deref() == Some(*unit))
        } else {
            run.contains(unit)
        };
        if !ran {
            problems.push(format!(
                "the package installs {unit}, and the entry runs neither it nor an instance of it"
            ));
        }
    }
    let program = sent["manifest"]["hook"]["command"][0]
        .as_str()
        .expect("a hook program");
    let executable = program.strip_prefix('/').is_some_and(|p| {
        files
            .iter()
            .any(|(path, mode)| path == p && mode & 0o111 == 0o111)
    });
    if !executable {
        problems.push(format!(
            "the hook runs {program}, which is not an executable file of the package"
        ));
    }
    problems
}

#[test]
fn the_run_units_and_hook_of_each_entry_are_the_unit_files_and_binary_of_the_deb_release_deb_builds()
 {
    // Catches: an entry whose run units are not what the package installs, read from the
    // package's own bytes: the entry of the earlier package (`tierstore.service`), which
    // hostkeeper refuses for the slot template's package; a template sent as a run unit; a unit
    // file of the package that the entry does not run; and a hook program the package does not
    // install as an executable. The package is the one `release deb` writes for each platform, and
    // the entry names its package and version as its control file does.
    let dir = scratch("built-deb");
    let binary = dir.join("tierstore");
    std::fs::write(&binary, b"\x7fELF stand-in").expect("binary");
    let licenses = dir.join("THIRD-PARTY-LICENSES.txt");
    std::fs::write(&licenses, b"notice\n").expect("notice");
    let out = dir.join("out");
    let mut built = 0;
    for platform in Platform::ALL {
        let Some(arch) = architecture(platform) else {
            continue;
        };
        super::super::deb::deb(&binary, "1.2.3", platform, &licenses, &out).expect("release deb");
        let bytes = std::fs::read(out.join(file_name("1.2.3", arch))).expect("the package");
        let sent = entry("1.2.3", &Sha256::of(&bytes).to_string(), FLOOR);
        let files = data_files(&bytes);
        assert!(
            files
                .iter()
                .any(|(p, _)| UNIT_DIRS.iter().any(|d| p.starts_with(d))),
            "{arch}: the package installs no unit file: {files:?}"
        );
        assert_eq!(
            install_problems(&sent, &files),
            Vec::<String>::new(),
            "{arch}"
        );
        let control = control::parse(&control_of(&bytes).expect("control")).expect("fields");
        assert_eq!(
            control::get(&control, "Package"),
            sent["manifest"]["package"].as_str(),
            "{arch}"
        );
        assert_eq!(
            control::get(&control, "Version"),
            sent["packageVersion"].as_str(),
            "{arch}"
        );
        built += 1;
    }
    assert_eq!(built, 2);
}

#[test]
fn an_entry_and_a_package_whose_units_differ_are_named_each_way() {
    // Catches: a comparison that passes whatever the package installs, so that the test above
    // could not fail on the entry of the earlier package or on a package without the template.
    let unit = |name: &str| (format!("usr/lib/systemd/system/{name}"), 0o644);
    let program = ("usr/libexec/tierstore/tierstore".to_owned(), 0o755);
    let template = [program.clone(), unit("tierstore@.service")];
    let current = entry("1.2.3", &digest_of("x"), FLOOR);
    let mut earlier = current.clone();
    earlier["manifest"]["run"] = json!(["tierstore.service"]);
    assert_eq!(
        install_problems(&earlier, &template),
        [
            "the entry runs tierstore.service, which is neither a unit file of the package nor an instance of one of its templates",
            "the package installs tierstore@.service, and the entry runs neither it nor an instance of it",
        ]
    );
    let single = [program.clone(), unit("tierstore.service")];
    assert_eq!(
        install_problems(&current, &single),
        [
            "the entry runs tierstore@a.service, which is neither a unit file of the package nor an instance of one of its templates",
            "the entry runs tierstore@b.service, which is neither a unit file of the package nor an instance of one of its templates",
            "the package installs tierstore.service, and the entry runs neither it nor an instance of it",
        ]
    );
    assert_eq!(install_problems(&earlier, &single), Vec::<String>::new());
    let mut both = current.clone();
    both["manifest"]["run"] = json!(["tierstore@.service", "tierstore@.service.d"]);
    assert_eq!(
        install_problems(&both, &template),
        [
            "the entry runs the template tierstore@.service, not an instance of it",
            "the entry runs tierstore@.service.d, which is neither a unit file of the package nor an instance of one of its templates",
            "the package installs tierstore@.service, and the entry runs neither it nor an instance of it",
        ]
    );
    let not_executable = [
        ("usr/libexec/tierstore/tierstore".to_owned(), 0o644),
        unit("tierstore@.service"),
    ];
    let mut relative = current.clone();
    relative["manifest"]["hook"]["command"][0] = json!("usr/libexec/tierstore/tierstore");
    for (sent, files) in [(&current, &not_executable[..]), (&relative, &template[..])] {
        let problems = install_problems(sent, files);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].ends_with("which is not an executable file of the package"),
            "{problems:?}"
        );
    }
}
