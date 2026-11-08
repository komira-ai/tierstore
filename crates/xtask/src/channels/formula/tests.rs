use super::*;
use crate::fetch::Sha256;
use crate::testdir::TestDir;

const REPO: &str = "komira-ai/tierstore";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside.parent().expect("deps").join("xtask-formula-unit"),
        name,
    )
}

fn hash(n: u8) -> String {
    Sha256::of(&[n]).to_string()
}

fn hashes() -> [String; 3] {
    [hash(1), hash(2), hash(3)]
}

/// The sums of a release of 1.2.3: the commit-named files beside the tap-named ones.
fn sums_text(drop: &[&str], twice: &[&str]) -> String {
    let mut lines = vec![sums::Line {
        sha256: Sha256::of(b"commit-named"),
        name: "tierstore-0123456789abcdef0123456789abcdef01234567-linux-x86_64.tar.gz".to_owned(),
    }];
    for (n, platform) in [
        Platform::MacosArm64,
        Platform::LinuxArm64,
        Platform::LinuxX86_64,
    ]
    .into_iter()
    .enumerate()
    {
        let name = file_name("1.2.3", platform);
        if drop.contains(&name.as_str()) {
            continue;
        }
        let line = sums::Line {
            sha256: Sha256::of(&[u8::try_from(n + 1).expect("small")]),
            name: name.clone(),
        };
        if twice.contains(&name.as_str()) {
            lines.push(line.clone());
        }
        lines.push(line);
    }
    sums::render(&lines)
}

#[test]
fn the_formula_is_the_text_the_taps_guard_takes_line_for_line() {
    // Catches: any drift from the guard's template (a version line, another order of the three
    // platforms, a url of another file name, a missing blank line), which makes the tap refuse
    // the pull request of every release.
    let want = format!(
        "class Tierstore < Formula
  desc \"S3 server over a hot and a cold tier of S3-compatible stores\"
  homepage \"https://tierstore.dev\"
  license \"Apache-2.0\"

  on_macos do
    on_arm do
      url \"https://github.com/komira-ai/tierstore/releases/download/v1.2.3/tierstore-1.2.3-macos-arm64.tar.gz\"
      sha256 \"{}\"
    end
  end

  on_linux do
    on_arm do
      url \"https://github.com/komira-ai/tierstore/releases/download/v1.2.3/tierstore-1.2.3-linux-aarch64.tar.gz\"
      sha256 \"{}\"
    end
    on_intel do
      url \"https://github.com/komira-ai/tierstore/releases/download/v1.2.3/tierstore-1.2.3-linux-x86_64.tar.gz\"
      sha256 \"{}\"
    end
  end

  def install
    bin.install \"tierstore\"
  end

  test do
    assert_match \"tierstore #{{version}}\", shell_output(\"#{{bin}}/tierstore --version\")
  end
end
",
        hash(1),
        hash(2),
        hash(3)
    );
    assert_eq!(render(REPO, "1.2.3", &hashes()), want);
}

#[test]
fn the_sums_of_each_tarball_come_from_the_release_sums_alone() {
    // Catches: a hash taken from the commit-named tarball or another platform's line, and a
    // formula that is written when something is wrong.
    let dir = scratch("render");
    let sums_file = dir.join("SHA256SUMS");
    std::fs::write(&sums_file, sums_text(&[], &[])).expect("sums");
    let out = dir.join("tap").join("Formula").join("tierstore.rb");
    let said = formula(&sums_file, "1.2.3", REPO, &out).expect("rendered");
    assert_eq!(
        said,
        [format!(
            "{}: the formula of 1.2.3, from the sums of {}",
            out.display(),
            sums_file.display()
        )]
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("formula"),
        render(REPO, "1.2.3", &hashes())
    );
}

#[test]
fn a_sums_file_that_lacks_a_tarball_or_lists_one_twice_is_refused() {
    // Catches: a formula with a hash missing or a guess in its place, and one that picks the
    // first of two lines.
    let dir = scratch("refused");
    let sums_file = dir.join("SHA256SUMS");
    let out = dir.join("tierstore.rb");
    std::fs::write(
        &sums_file,
        sums_text(
            &["tierstore-1.2.3-macos-arm64.tar.gz"],
            &["tierstore-1.2.3-linux-x86_64.tar.gz"],
        ),
    )
    .expect("sums");
    assert_eq!(
        formula(&sums_file, "1.2.3", REPO, &out).unwrap_err(),
        Error::Refused(vec![
            "SHA256SUMS does not list tierstore-1.2.3-macos-arm64.tar.gz".to_owned(),
            "SHA256SUMS lists tierstore-1.2.3-linux-x86_64.tar.gz twice".to_owned(),
        ])
    );
    assert!(!out.exists());
    // The sums of another version name other files.
    std::fs::write(&sums_file, sums_text(&[], &[])).expect("sums");
    let Error::Refused(problems) = formula(&sums_file, "1.2.4", REPO, &out).unwrap_err() else {
        panic!("refused")
    };
    assert_eq!(problems.len(), 3);
}

#[test]
fn flags_out_of_their_form_and_files_that_cannot_be_made_or_read_fail() {
    // Catches: a version or repository passed through unchecked, a formula written over, and a
    // sums file that is not there.
    let dir = scratch("flags");
    let sums_file = dir.join("SHA256SUMS");
    let out = dir.join("tierstore.rb");
    std::fs::write(&sums_file, sums_text(&[], &[])).expect("sums");
    assert_eq!(
        formula(&sums_file, "1.2", REPO, &out).unwrap_err(),
        Error::Usage("--version \"1.2\" is not <major>.<minor>.<patch>".to_owned())
    );
    for repo in [
        "komira-ai/other",
        "tierstore",
        "/tierstore",
        "a b/tierstore",
    ] {
        assert_eq!(
            formula(&sums_file, "1.2.3", repo, &out).unwrap_err(),
            Error::Usage(format!("--repo {repo:?} is not <owner>/tierstore"))
        );
    }
    assert!(matches!(
        formula(&dir.join("missing"), "1.2.3", REPO, &out).unwrap_err(),
        Error::Spawn { .. }
    ));
    formula(&sums_file, "1.2.3", REPO, &out).expect("first");
    assert!(matches!(
        formula(&sums_file, "1.2.3", REPO, &out).unwrap_err(),
        Error::Spawn { .. }
    ));
    // The root has no directory above it and cannot be a new file.
    assert!(matches!(
        formula(&sums_file, "1.2.3", REPO, Path::new("/")).unwrap_err(),
        Error::Spawn { .. }
    ));
    // The directory of the formula is a file.
    let under_a_file = sums_file.join("tierstore.rb");
    assert!(matches!(
        formula(&sums_file, "1.2.3", REPO, &under_a_file).unwrap_err(),
        Error::Spawn { .. }
    ));
}

#[test]
fn check_accepts_the_rendered_formula_and_only_it() {
    // Catches: a formula another tool wrote or a person edited reaching the tap's guard, which
    // refuses anything but the template.
    let good = render(REPO, "1.2.3", &hashes());
    assert_eq!(check(&good, REPO, "1.2.3"), Ok(()));
    let bad_hash = good.replace(&hash(2), &hash(2).to_uppercase());
    let one_short = good.replacen("      sha256 \"", "      sha 256 \"", 1);
    let cases = [
        (
            good.replace(
                "v1.2.3/tierstore-1.2.3-macos",
                "v1.2.3/tierstore-1.2.4-macos",
            ),
            "is not the formula of 1.2.3 of komira-ai/tierstore that `channels formula` writes",
        ),
        (
            good.replace("  license", "  version \"1.2.3\"\n  license"),
            "is not the formula of 1.2.3 of komira-ai/tierstore that `channels formula` writes",
        ),
        (
            good.replace("bin.install", "system \"curl\"\n    bin.install"),
            "is not the formula of 1.2.3 of komira-ai/tierstore that `channels formula` writes",
        ),
        (
            good.replace("komira-ai", "someone"),
            "is not the formula of 1.2.3 of komira-ai/tierstore that `channels formula` writes",
        ),
        (
            good.clone() + "\n",
            "is not the formula of 1.2.3 of komira-ai/tierstore that `channels formula` writes",
        ),
        (one_short, "holds 2 sha256 lines, not 3"),
        (String::new(), "holds 0 sha256 lines, not 3"),
        (
            bad_hash.clone(),
            &format!(
                "holds the sha256 {:?}, not 64 lowercase hex digits",
                hash(2).to_uppercase()
            ),
        ),
    ];
    for (text, why) in cases {
        assert_eq!(check(&text, REPO, "1.2.3"), Err(why.to_owned()), "{text}");
    }
    assert_eq!(
        check(&good, REPO, "1.2.4"),
        Err(
            "is not the formula of 1.2.4 of komira-ai/tierstore that `channels formula` writes"
                .to_owned()
        )
    );
}

#[test]
fn the_version_of_a_live_formula_is_read_from_its_urls() {
    // Catches: a tap that moves down because the live version was read from the wrong place,
    // and a live formula with urls of two versions or of another repository read as one.
    let live = render(REPO, "1.2.3", &hashes());
    assert_eq!(live_version(&live, REPO), Ok("1.2.3".to_owned()));
    let mixed = live.replacen(
        "v1.2.3/tierstore-1.2.3-macos",
        "v1.2.4/tierstore-1.2.4-macos",
        1,
    );
    assert_eq!(
        live_version(&mixed, REPO),
        Err("its urls name different versions".to_owned())
    );
    let foreign = live.replace("komira-ai/tierstore", "komira-ai/other");
    assert!(
        live_version(&foreign, REPO)
            .unwrap_err()
            .contains("is not a release url of komira-ai/tierstore"),
    );
    let skewed = live.replacen("tierstore-1.2.3-macos", "tierstore-1.2.9-macos", 1);
    assert!(live_version(&skewed, REPO).is_err());
    let leading_zero = live.replace("v1.2.3/tierstore-1.2.3", "v1.02.3/tierstore-1.02.3");
    assert!(live_version(&leading_zero, REPO).is_err());
    let no_gz = live.replacen(".tar.gz\"", ".zip\"", 1);
    assert!(live_version(&no_gz, REPO).is_err());
    let no_slash =
        "      url \"https://github.com/komira-ai/tierstore/releases/download/v1.2.3\"\n";
    assert!(live_version(no_slash, REPO).is_err());
    assert_eq!(
        live_version("class Tierstore < Formula\nend\n", REPO),
        Err("it has no url line".to_owned())
    );
}

#[test]
fn only_the_released_repository_of_that_name_is_a_repository() {
    // Catches: a formula named for a repository the guard builds no url for.
    assert!(is_repo("komira-ai/tierstore"));
    assert!(is_repo("a_b.c/tierstore"));
    assert!(!is_repo("komira-ai/tierstore2"));
    assert!(!is_repo("/tierstore"));
    assert!(!is_repo("komira ai/tierstore"));
    assert!(!is_repo("tierstore"));
}
