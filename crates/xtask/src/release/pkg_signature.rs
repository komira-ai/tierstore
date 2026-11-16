//! `release check-pkg-signature <pkg> --identity <name>`: refuses the installer pkg unless macOS
//! judges its signature as a release's must be. It reads what `pkgutil --check-signature` prints
//! of the pkg (through [`BuildPkg::check_signature`], which fails when `pkgutil` exits other than 0) and refuses it, naming every reason, unless:
//!
//! 1. its `Status:` is one of [`TRUSTED_STATUSES`], the words `pkgutil` uses for a signature whose
//!    chain the Mac trusts (they differ between macOS versions);
//! 2. it was signed with a trusted timestamp (the release signs with `productsign --timestamp`, so
//!    the signature stays valid after the certificate expires);
//! 3. its certificate chain is exactly the identity, then [`INTERMEDIATE`], then [`ROOT`]:
//!    the Developer ID Installer identity, Apple's Developer ID intermediate and Apple's root.
//!
//! The identity must be a Developer ID Installer identity ([`IDENTITY_PREFIX`]); any other is
//! refused before `pkgutil` runs.

use std::path::Path;

use super::Error;
use super::macos_pkg::BuildPkg;

/// The statuses `pkgutil --check-signature` prints for a signature whose chain the Mac trusts:
/// a Developer ID signature on current macOS, and the wording of earlier releases.
pub const TRUSTED_STATUSES: [&str; 3] = [
    "signed by a developer certificate issued by Apple for distribution",
    "signed by a certificate trusted by macOS",
    "signed by a certificate trusted by Mac OS X",
];

/// How `pkgutil` names the trusted timestamp of a signature, before the time.
pub const TIMESTAMP: &str = "Signed with a trusted timestamp on:";

/// The common name of Apple's intermediate of every Developer ID certificate.
pub const INTERMEDIATE: &str = "Developer ID Certification Authority";

/// The common name of Apple's root.
pub const ROOT: &str = "Apple Root CA";

/// What the name of a Developer ID Installer identity starts with.
pub const IDENTITY_PREFIX: &str = "Developer ID Installer: ";

/// Checks the signature of `pkg` against `identity` through `tools`.
pub fn check(tools: &dyn BuildPkg, pkg: &Path, identity: &str) -> Result<Vec<String>, Error> {
    if !identity.starts_with(IDENTITY_PREFIX) || identity.len() == IDENTITY_PREFIX.len() {
        return Err(Error::Failed(format!(
            "--identity {identity:?} is not a Developer ID Installer identity ({IDENTITY_PREFIX}<name> (<team>))"
        )));
    }
    let report = tools.check_signature(pkg)?;
    let problems = problems(&report, identity);
    if !problems.is_empty() {
        return Err(Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", pkg.display()))
                .collect(),
        ));
    }
    Ok(vec![format!(
        "{}: signed by {identity} under {INTERMEDIATE} and {ROOT}, with a trusted timestamp",
        pkg.display()
    )])
}

/// One line per way `report`, what `pkgutil --check-signature` printed, is not a trusted
/// signature by `identity` with a trusted timestamp.
pub fn problems(report: &str, identity: &str) -> Vec<String> {
    let lines: Vec<&str> = report.lines().map(str::trim).collect();
    let mut problems = Vec::new();
    let statuses: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("Status:"))
        .map(str::trim)
        .collect();
    match statuses[..] {
        [status] if TRUSTED_STATUSES.contains(&status) => {}
        [status] => problems.push(format!(
            "the status is {status:?}, not a trusted signature ({TRUSTED_STATUSES:?})"
        )),
        _ => problems.push(format!(
            "pkgutil printed {} status lines, not one",
            statuses.len()
        )),
    }
    if !lines.iter().any(|l| l.starts_with(TIMESTAMP)) {
        problems.push(format!(
            "the signature has no trusted timestamp ({TIMESTAMP:?})"
        ));
    }
    let chain = chain(&lines);
    let want = [identity, INTERMEDIATE, ROOT];
    if chain != want {
        problems.push(format!("the certificate chain is {chain:?}, not {want:?}"));
    }
    problems
}

/// The names of the certificate chain, in order: each line after `Certificate Chain:` that is a
/// number, a dot and a space, then the name.
fn chain<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let Some(start) = lines.iter().position(|l| *l == "Certificate Chain:") else {
        return Vec::new();
    };
    lines[start + 1..]
        .iter()
        .filter_map(|l| {
            let (number, name) = l.split_once(". ")?;
            let numbered = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
            numbered.then_some(name)
        })
        .collect()
}

#[cfg(test)]
mod tests;
