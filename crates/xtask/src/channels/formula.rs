//! `channels formula --sums <file> --version <X.Y.Z> --repo <owner/tierstore> --out <file>`: the
//! Homebrew formula of a release, `Formula/tierstore.rb`.
//!
//! The text is the one the tap's guard (`.github/guard/guard.js` of `homebrew-tap`, `template`)
//! takes, line for line: three `url` and `sha256` pairs under `on_macos` and `on_linux` (`on_arm`,
//! `on_intel`), `bin.install "tierstore"`, and a test that wants `tierstore <version>` in the output
//! of `tierstore --version`. There is no `version` line: brew reads the version from the url, and
//! `brew audit --strict` refuses a line that repeats it. The urls are the tap-named tarballs of
//! the GitHub release ([`crate::release::tap_tarball`]), and each `sha256` is the one the
//! release's `SHA256SUMS` lists for that file: nothing is downloaded and nothing is hashed here, so
//! the formula says what the verified sums say. A sums file that lacks one of the three tarballs,
//! or lists one twice, is refused.

use std::path::Path;

use crate::Error;
use crate::package::{Platform, write_new};
use crate::release::deb::{HOMEPAGE, SYNOPSIS};
use crate::release::hostkeeper_package::is_plain_version;
use crate::release::tap_tarball::file_name;
use crate::sums::{self, SUMS};

/// The formula's name, the program it installs and the name of the released repository.
pub const NAME: &str = "tierstore";

/// The licence the formula declares, as the crate's manifest has it.
pub const LICENSE: &str = "Apache-2.0";

/// The platforms in the order the formula lists them: `(block, cpu block, platform)`.
const TARGETS: [(&str, &str, Platform); 3] = [
    ("macos", "arm", Platform::MacosArm64),
    ("linux", "arm", Platform::LinuxArm64),
    ("linux", "intel", Platform::LinuxX86_64),
];

/// The marker of a line that holds a sum.
const SHA_LINE: &str = "      sha256 \"";

/// The text of the formula of `version` of `repo` (`<owner>/tierstore`), with the three sums in
/// the order of [`TARGETS`].
pub fn render(repo: &str, version: &str, hashes: &[String; 3]) -> String {
    let base = format!("https://github.com/{repo}/releases/download/v{version}");
    let mut text = format!(
        "class Tierstore < Formula\n  desc \"{SYNOPSIS}\"\n  homepage \"{HOMEPAGE}\"\n  license \"{LICENSE}\"\n"
    );
    for block in ["macos", "linux"] {
        text.push_str(&format!("\n  on_{block} do\n"));
        for ((_, cpu, platform), hash) in TARGETS
            .iter()
            .zip(hashes)
            .filter(|((b, ..), _)| *b == block)
        {
            text.push_str(&format!(
                "    on_{cpu} do\n      url \"{base}/{}\"\n{SHA_LINE}{hash}\"\n    end\n",
                file_name(version, *platform)
            ));
        }
        text.push_str("  end\n");
    }
    text.push_str(&format!(
        "\n  def install\n    bin.install \"{NAME}\"\n  end\n\n  test do\n    assert_match \"{NAME} #{{version}}\", shell_output(\"#{{bin}}/{NAME} --version\")\n  end\nend\n"
    ));
    text
}

/// Whether `repo` is `<owner>/tierstore`: the guard builds the urls from the tap's owner and the
/// formula's name.
pub fn is_repo(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(owner, name)| {
        name == NAME
            && !owner.is_empty()
            && owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    })
}

/// Why `text` is not the formula of `version` of `repo`: the three sums are read from it, and
/// the text must be exactly what [`render`] writes for them.
pub fn check(text: &str, repo: &str, version: &str) -> Result<(), String> {
    let found: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix(SHA_LINE)?.strip_suffix('"'))
        .map(str::to_owned)
        .collect();
    let [macos, linux_arm, linux_x86] = <[String; 3]>::try_from(found)
        .map_err(|f| format!("holds {} sha256 lines, not 3", f.len()))?;
    let hashes = [macos, linux_arm, linux_x86];
    if let Some(bad) = hashes
        .iter()
        .find(|h| h.parse::<crate::fetch::Sha256>().is_err())
    {
        return Err(format!(
            "holds the sha256 {bad:?}, not 64 lowercase hex digits"
        ));
    }
    if text == render(repo, version, &hashes) {
        Ok(())
    } else {
        Err(format!(
            "is not the formula of {version} of {repo} that `channels formula` writes"
        ))
    }
}

/// The version a formula's urls name, as the tap's guard reads it from the base: every `url` line
/// is a release url of `repo` whose directory version and file name version agree, and they all
/// name one version.
pub fn live_version(text: &str, repo: &str) -> Result<String, String> {
    let head = format!("      url \"https://github.com/{repo}/releases/download/v");
    let mut found: Option<&str> = None;
    for line in text.lines().filter(|l| l.trim_start().starts_with("url ")) {
        let version = line
            .strip_prefix(&head)
            .and_then(|rest| rest.split_once('/'))
            .filter(|(v, file)| {
                is_plain_version(v)
                    && file.starts_with(&format!("{NAME}-{v}-"))
                    && file.ends_with(".tar.gz\"")
            })
            .map(|(v, _)| v)
            .ok_or_else(|| {
                format!(
                    "the url line {:?} is not a release url of {repo}",
                    line.trim()
                )
            })?;
        if found.is_some_and(|f| f != version) {
            return Err("its urls name different versions".to_owned());
        }
        found = Some(version);
    }
    found
        .map(str::to_owned)
        .ok_or_else(|| "it has no url line".to_owned())
}

/// The `formula` verb: the line to print.
///
/// # Errors
/// A version or repository out of its form, a sums file that is unreadable, lacks one of the three
/// tarballs or lists one twice, or a formula that exists already.
pub fn formula(
    sums_file: &Path,
    version: &str,
    repo: &str,
    out: &Path,
) -> Result<Vec<String>, Error> {
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    if !is_repo(repo) {
        return Err(Error::Usage(format!(
            "--repo {repo:?} is not <owner>/{NAME}"
        )));
    }
    let lines = sums::read_sums(sums_file)?;
    let mut problems = Vec::new();
    let mut hashes = Vec::new();
    for (_, _, platform) in TARGETS {
        let name = file_name(version, platform);
        let mut listed = lines.iter().filter(|l| l.name == name);
        match (listed.next(), listed.next()) {
            (Some(line), None) => hashes.push(line.sha256.to_string()),
            (None, _) => problems.push(format!("{SUMS} does not list {name}")),
            (Some(_), Some(_)) => problems.push(format!("{SUMS} lists {name} twice")),
        }
    }
    let Ok(hashes) = <[String; 3]>::try_from(hashes) else {
        return Err(Error::Refused(problems));
    };
    let parent = out.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(Error::spawn(format!("create {}", parent.display())))?;
    write_new(out, render(repo, version, &hashes).as_bytes())?;
    Ok(vec![format!(
        "{}: the formula of {version}, from the sums of {}",
        out.display(),
        sums_file.display()
    )])
}

#[cfg(test)]
mod tests;
