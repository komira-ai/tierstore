//! The signed macOS installer in `artifacts.yml`, as the workflow lint holds it
//! ([`super::platforms`] calls these):
//!
//! - the macOS build job builds it unsigned into `pkg/`, expands it and reads it back
//!   ([`MACOS_PKG_COMMANDS`]), and uploads it as [`PKG_UNSIGNED`];
//! - `sign-pkg` is the one job that holds the installer identity, on a push to main alone, after
//!   whichever macOS build ran (hosted or on the farm, the same artifact), in the
//!   environment `pkg-signing`, with `contents: read`, and runs no repository code: no checkout,
//!   no cargo, exactly [`SIGN_PKG_COMMANDS`] and [`SIGN_PKG_CLEANUP`] ([`sign_pkg_problems`]);
//! - `check-pkg` runs on a push to main once `sign-pkg` passed (`macos_farm::CHECK_PKG_IF`),
//!   holds no secret and no environment, runs exactly [`CHECK_PKG_COMMANDS`] and
//!   uploads the checked installer as [`PKG_ASSETS`], the release file ([`check_pkg_problems`]);
//! - no other job names a secret ([`secret_problems`]);
//! - the sums, the signing and the checks take it with the other files ([`assets_of`],
//!   [`pkg_attest_problems`]).

use super::lint::{entries, entry, names, scalars};
use super::platforms::{
    DOWNLOAD, UPLOAD, VERSION_OUTPUT, action_problems, at, command_problems, commands, steps, using,
};
use super::yaml::Node;

/// The macOS job's last commands: the installer built unsigned into `pkg/` (not `dist/`: it is no
/// release file until it is signed and checked), expanded, and read back against the builder.
pub const MACOS_PKG_COMMANDS: [&str; 4] = [
    "mkdir pkg",
    "cargo xtask release macos-pkg --version \"$RELEASE_VERSION\" --binary \"target/$TARGET/release/tierstore\" --stage \"$RUNNER_TEMP/pkg-root\" --out pkg",
    "pkgutil --expand \"pkg/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" \"$RUNNER_TEMP/pkg-expanded\"",
    "cargo xtask release check-macos-pkg \"$RUNNER_TEMP/pkg-expanded\" --version \"$RELEASE_VERSION\" --binary \"target/$TARGET/release/tierstore\"",
];

/// The artifact the unsigned installer is uploaded as, by the macOS job, and the one the signed
/// installer is, by `sign-pkg`: neither is a file a release waits for (`xtask::release::ARTIFACTS`
/// lists `assets-pkg-macos-arm64`, which `check-pkg` makes of the signed one alone).
pub const PKG_UNSIGNED: &str = "pkg-unsigned-macos-arm64";
pub const PKG_SIGNED: &str = "pkg-signed-macos-arm64";
/// The artifact `check-pkg` uploads: the signed installer, as a release file.
pub const PKG_ASSETS: &str = "assets-pkg-macos-arm64";

/// The identity the installer is signed with, and the one `check-pkg-signature` holds the
/// signature to.
pub const PKG_IDENTITY: &str = "Developer ID Installer: Komira AI, Inc. (U7RTX4XD3X)";

/// The `run:` commands of `sign-pkg`, in order. No checkout and no cargo: no repository code runs
/// where the identity is.
pub const SIGN_PKG_COMMANDS: [&str; 9] = [
    "base64 --decode -o \"$RUNNER_TEMP/installer.p12\" <<< \"$P12\"",
    "security create-keychain -p \"$PASSWORD\" \"$RUNNER_TEMP/pkg-signing.keychain-db\"",
    "security set-keychain-settings -lut 21600 \"$RUNNER_TEMP/pkg-signing.keychain-db\"",
    "security unlock-keychain -p \"$PASSWORD\" \"$RUNNER_TEMP/pkg-signing.keychain-db\"",
    "security import \"$RUNNER_TEMP/installer.p12\" -k \"$RUNNER_TEMP/pkg-signing.keychain-db\" -f pkcs12 -P \"$PASSWORD\" -T /usr/bin/productsign",
    "security set-key-partition-list -S \"apple-tool:,apple:\" -s -k \"$PASSWORD\" \"$RUNNER_TEMP/pkg-signing.keychain-db\"",
    "security list-keychains -d user -s \"$RUNNER_TEMP/pkg-signing.keychain-db\"",
    "mkdir dist",
    "productsign --sign \"$IDENTITY\" --keychain \"$RUNNER_TEMP/pkg-signing.keychain-db\" --timestamp \"unsigned/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" \"dist/tierstore-$RELEASE_VERSION-macos-arm64.pkg\"",
];

/// The two steps of `sign-pkg` that run whether or not the signing worked, and delete what the
/// identity left on the runner: the only steps of the file with `if: ${{ always() }}`.
pub const SIGN_PKG_CLEANUP: [&str; 2] = [
    "security delete-keychain \"$RUNNER_TEMP/pkg-signing.keychain-db\"",
    "rm -f \"$RUNNER_TEMP/installer.p12\"",
];

/// The `run:` commands of `check-pkg`, in order (after the checkout).
pub const CHECK_PKG_COMMANDS: [&str; 5] = [
    "rustup toolchain install",
    "cargo xtask release check-pkg-signature \"dist/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" --identity \"$IDENTITY\"",
    "pkgutil --expand \"unsigned/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" \"$RUNNER_TEMP/unsigned-expanded\"",
    "pkgutil --expand \"dist/tierstore-$RELEASE_VERSION-macos-arm64.pkg\" \"$RUNNER_TEMP/signed-expanded\"",
    "diff -r \"$RUNNER_TEMP/unsigned-expanded\" \"$RUNNER_TEMP/signed-expanded\"",
];

/// The copy of the bundle of the signed macOS installer under the installer's name.
pub fn pkg_copy() -> String {
    "cp \"$BUNDLE\" \"dist/tierstore-${RELEASE_VERSION}-macos-arm64.pkg.sigstore.json\"".to_owned()
}

/// The platforms whose `assets-<name>` a job takes: each of `platforms`, and `pkg-macos-arm64`, the
/// signed macOS installer, when `macos-arm64` is one of them.
pub fn assets_of<'a>(platforms: &[&'a str]) -> Vec<&'a str> {
    let mut all = platforms.to_vec();
    if platforms.contains(&"macos-arm64") {
        all.push("pkg-macos-arm64");
    }
    all
}

/// The `with:` inputs of the steps of `job` that use `action`, as `(name, path)`.
pub fn artifact_steps<'a>(
    job: Option<&'a Node>,
    action: &str,
) -> Vec<(Option<&'a str>, Option<&'a str>)> {
    using(job, action)
        .iter()
        .map(|s| (at(s, &["with", "name"]), at(s, &["with", "path"])))
        .collect()
}

/// One line per way the job `name` is not the macOS job it must be: on `macos-15`, needing `needs`,
/// reading at most `contents` of the repository, taking the artifacts `downloads` into their
/// directories and uploading `uploads`, using no action but the ones in `allowed`.
pub fn pkg_job_problems(
    name: &str,
    job: Option<&Node>,
    needs: &[&str],
    downloads: &[(Option<&str>, Option<&str>)],
    uploads: &[(Option<&str>, Option<&str>)],
    allowed: &[&str],
) -> Vec<String> {
    let Some(job) = job else {
        return vec![format!("{name}: no such job")];
    };
    let mut problems = Vec::new();
    if at(job, &["runs-on"]) != Some("macos-15") {
        problems.push(format!("{name}: does not run on macos-15"));
    }
    let mut want_needs = needs.to_vec();
    want_needs.sort_unstable();
    if names(entry(job, "needs")) != want_needs {
        problems.push(format!("{name}: does not need exactly {needs:?}"));
    }
    let permissions: Vec<(&str, &str)> = entry(job, "permissions")
        .map_or(&[][..], entries)
        .iter()
        .filter_map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
        .collect();
    if permissions != [("contents", "read")] {
        problems.push(format!(
            "{name}: permissions are {permissions:?}, not `contents: read` alone"
        ));
    }
    if artifact_steps(Some(job), DOWNLOAD) != downloads {
        problems.push(format!(
            "{name}: downloads {:?}, not {downloads:?}",
            artifact_steps(Some(job), DOWNLOAD)
        ));
    }
    if artifact_steps(Some(job), UPLOAD) != uploads {
        problems.push(format!(
            "{name}: uploads {:?}, not {uploads:?}",
            artifact_steps(Some(job), UPLOAD)
        ));
    }
    problems.extend(action_problems(name, Some(job), allowed));
    let identity = at(job, &["env", "IDENTITY"]);
    if identity != Some(PKG_IDENTITY) {
        problems.push(format!(
            "{name}: IDENTITY is {identity:?}, not {PKG_IDENTITY:?}"
        ));
    }
    problems
}

/// One line per way `sign-pkg` is not the one job that holds the installer identity and runs no
/// repository code: in the environment `pkg-signing`, `contents: read` alone, no checkout and no
/// cargo (only the two artifact actions and exactly [`SIGN_PKG_COMMANDS`] and [`SIGN_PKG_CLEANUP`]),
/// the secrets read only by the steps that import the identity, and the two cleanup steps (and no
/// other) running under `if: ${{ always() }}`.
pub fn sign_pkg_problems(job: Option<&Node>) -> Vec<String> {
    let mut problems = pkg_job_problems(
        "sign-pkg",
        job,
        &["version", "macos-arm64", super::macos_farm::CALLER],
        &[(Some(PKG_UNSIGNED), Some("unsigned"))],
        &[(Some(PKG_SIGNED), Some("dist/"))],
        &[DOWNLOAD, UPLOAD],
    );
    let Some(job) = job else {
        return problems;
    };
    if at(job, &["environment"]) != Some("pkg-signing") {
        problems.push("sign-pkg: is not in the environment pkg-signing".to_owned());
    }
    let mut want: Vec<String> = SIGN_PKG_COMMANDS.map(str::to_owned).to_vec();
    want.extend(SIGN_PKG_CLEANUP.map(str::to_owned));
    problems.extend(command_problems("sign-pkg", &commands(Some(job)), &want));
    let cleaning: Vec<&str> = steps(Some(job))
        .iter()
        .filter(|s| at(s, &["if"]) == Some("${{ always() }}"))
        .filter_map(|s| at(s, &["run"]))
        .map(str::trim_end)
        .collect();
    if cleaning != SIGN_PKG_CLEANUP {
        problems.push(format!(
            "sign-pkg: the steps under `if: ${{{{ always() }}}}` run {cleaning:?}, not {SIGN_PKG_CLEANUP:?}"
        ));
    }
    if steps(Some(job))
        .iter()
        .any(|s| entry(s, "if").is_some() && at(s, &["if"]) != Some("${{ always() }}"))
    {
        problems.push("sign-pkg: a step has an `if:` other than `${{ always() }}`".to_owned());
    }
    problems
}

/// One line per way `check-pkg` is not the job that checks the signed installer and holds no
/// secret: no environment, `contents: read` alone, exactly [`CHECK_PKG_COMMANDS`], the unsigned
/// and the signed installer in, the checked one out as the release file.
pub fn check_pkg_problems(job: Option<&Node>) -> Vec<String> {
    let mut problems = pkg_job_problems(
        "check-pkg",
        job,
        &["version", "sign-pkg"],
        &[
            (Some(PKG_UNSIGNED), Some("unsigned")),
            (Some(PKG_SIGNED), Some("dist")),
        ],
        &[(Some(PKG_ASSETS), Some("dist/"))],
        &["actions/checkout@", DOWNLOAD, UPLOAD],
    );
    let Some(job) = job else {
        return problems;
    };
    if entry(job, "environment").is_some() {
        problems.push("check-pkg: is in an environment; it holds no secret".to_owned());
    }
    let want = CHECK_PKG_COMMANDS.map(str::to_owned).to_vec();
    problems.extend(command_problems("check-pkg", &commands(Some(job)), &want));
    problems
}

/// One line per way the jobs of the installer's path are not as the module says: `sign-pkg`,
/// `check-pkg`, the secrets, and `sums` (its `needs`, `sums_needs`, lack `check-pkg`), for a file
/// that builds `macos-arm64`.
pub fn pkg_jobs_problems(
    platforms: &[&str],
    jobs: &[(Node, Node)],
    sums_needs: &[String],
) -> Vec<String> {
    if !platforms.contains(&"macos-arm64") {
        return Vec::new();
    }
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut problems = sign_pkg_problems(job("sign-pkg"));
    problems.extend(check_pkg_problems(job("check-pkg")));
    problems.extend(secret_problems(jobs));
    if !sums_needs.iter().any(|n| n == "check-pkg") {
        problems.push("sums: does not need check-pkg".to_owned());
    }
    problems
}

/// One line per job but `sign-pkg` that names a secret: the installer identity is read by that job
/// alone.
pub fn secret_problems(jobs: &[(Node, Node)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        if name == "sign-pkg" {
            continue;
        }
        let mut found = Vec::new();
        scalars(job, &mut found);
        if found.iter().any(|s| s.contains("secrets.")) {
            problems.push(format!("{name}: reads a secret; sign-pkg alone does"));
        }
    }
    problems
}

/// One line per way `attest` does not sign the signed macOS installer as its own subject, by the
/// name the `version` job's output gives it, and copy that attestation's bundle under its name.
pub fn pkg_attest_problems(attest: Option<&Node>, provenance_steps: &[&Node]) -> Vec<String> {
    let mut problems = Vec::new();
    let subject = format!("dist/tierstore-{VERSION_OUTPUT}-macos-arm64.pkg");
    let id = "provenance-pkg-macos-arm64";
    let own = provenance_steps.iter().any(|s| {
        at(s, &["id"]) == Some(id) && at(s, &["with", "subject-path"]) == Some(subject.as_str())
    });
    if !own {
        problems.push(format!(
            "attest: no provenance attestation `{id}` of {subject} alone"
        ));
    }
    let output = format!("${{{{ steps.{id}.outputs.bundle-path }}}}");
    let copy = pkg_copy();
    let copied = steps(attest).iter().any(|s| {
        at(s, &["env", "BUNDLE"]) == Some(output.as_str())
            && at(s, &["env", "RELEASE_VERSION"]) == Some(VERSION_OUTPUT)
            && at(s, &["run"]).is_some_and(|r| r.trim_end() == copy)
    });
    if !copied {
        problems.push(format!(
            "attest: the bundle of `{id}` is not copied as `{copy}` from {output} with RELEASE_VERSION {VERSION_OUTPUT}"
        ));
    }
    problems
}
