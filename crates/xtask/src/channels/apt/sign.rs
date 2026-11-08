//! `channels apt-sign --dir <dir> --work <dir> --fingerprint <fpr>[,<fpr>...] --key-file <file>
//! --passphrase-file <file> [--gpg <program>] [--output <file>]`: `InRelease` (clear signed) and
//! `Release.gpg` (detached) of `<dir>/Release`, verified.
//!
//! The index is signed with the signing subkey of the archive key: the key spec is the subkey's
//! fingerprint followed by `!`, which makes gpg use exactly that key and no other. Several
//! fingerprints (a key rotation) give one signature per key, in both files. The passphrase is
//! never an argument (arguments are readable by every process of the machine): it is a file, with
//! the loopback pinentry. The key and passphrase files are written by the job from its secrets;
//! they and the keyring in a private directory (0700) below `--work` are removed when the verb
//! ends, whatever happened. A fingerprint that is not among the secret keys imported fails before
//! anything is signed. `signed=true` is appended to `--output` when both files verify.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;
use crate::channels::aws::Answer;

/// The `gpg` program, behind a trait so a test answers in place of it.
pub trait Gpg {
    /// `gpg <args>`. A refusal is an [`Answer`] that is not `ok`.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, args: &[String]) -> Result<Answer, Error>;
}

/// The `gpg` program at a path.
pub struct RealGpg(pub PathBuf);

impl Gpg for RealGpg {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(&self.0)
            .args(args)
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// What `apt-sign` is asked.
pub struct Request<'a> {
    pub dir: &'a Path,
    pub work: &'a Path,
    pub fingerprints: &'a str,
    pub key_file: &'a Path,
    pub passphrase_file: &'a Path,
}

/// A fingerprint as hex digits (the spaces gpg prints between groups are tolerated), at least 16
/// of them: upper case, without spaces.
fn clean_fingerprint(raw: &str) -> Option<String> {
    let clean: String = raw.chars().filter(|c| *c != ' ').collect();
    (clean.len() >= 16 && clean.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| clean.to_ascii_uppercase())
}

/// The fingerprints of the signing subkeys: one, or several separated by commas. A list that is
/// empty, has an empty element or names a key twice is refused.
fn split_fingerprints(raw: &str) -> Result<Vec<String>, Error> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        match clean_fingerprint(part) {
            Some(fpr) if !out.contains(&fpr) => out.push(fpr),
            _ => {
                return Err(Error::Usage(
                    "--fingerprint is not a fingerprint, or a comma-separated list of different ones"
                        .to_owned(),
                ));
            }
        }
    }
    Ok(out)
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The gpg half: imports the key, checks every fingerprint is a secret key, signs both files and
/// verifies them.
fn gpg_steps(
    gpg: &dyn Gpg,
    home: &Path,
    fingerprints: &[String],
    r: &Request,
) -> Result<(), Error> {
    let text = |p: &Path| p.display().to_string();
    let home_arg = text(home);
    let base = |extra: &[&str]| {
        let mut v = args(&["--homedir", &home_arg, "--batch", "--no-tty", "--yes"]);
        v.extend(args(extra));
        v
    };
    let (release, in_release, detached) = (
        text(&r.dir.join("Release")),
        text(&r.dir.join("InRelease")),
        text(&r.dir.join("Release.gpg")),
    );
    let run = |what: &str, argv: Vec<String>| -> Result<(), Error> {
        let answer = gpg.run(&argv)?;
        if answer.ok {
            Ok(())
        } else {
            Err(Error::Failed(format!("{what}: {}", answer.stderr.trim())))
        }
    };
    run(
        "gpg could not import the key",
        base(&["--import", &text(r.key_file)]),
    )?;
    for fpr in fingerprints {
        run(
            &format!("the signing key is not among the secret keys that were imported ({fpr})"),
            base(&["--list-secret-keys", "--with-colons", fpr]),
        )?;
    }
    let signing = |mode: &str, input: &str, output: &str| {
        let mut v = base(&[
            "--pinentry-mode",
            "loopback",
            "--passphrase-file",
            &text(r.passphrase_file),
            "--digest-algo",
            "SHA512",
        ]);
        for fpr in fingerprints {
            v.extend(args(&["--local-user", &format!("{fpr}!")]));
        }
        v.extend(args(&["--armor", mode, "--output", output, input]));
        v
    };
    run(
        "gpg could not sign InRelease",
        signing("--clearsign", &release, &in_release),
    )?;
    run(
        "gpg could not sign Release.gpg",
        signing("--detach-sign", &release, &detached),
    )?;
    run(
        "InRelease does not verify",
        base(&["--verify", &in_release]),
    )?;
    run(
        "Release.gpg does not verify",
        base(&["--verify", &detached, &release]),
    )
}

/// An empty directory `home` (mode 0700) below `work`, which is made when missing.
fn private_dir(work: &Path, home: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(home) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::create_dir_all(work)?;
    std::fs::DirBuilder::new().mode(0o700).create(home)
}

/// The `apt-sign` verb: the lines to print.
///
/// # Errors
/// A fingerprint that is not one, a `Release` that is not there, a private directory that cannot
/// be made, or a gpg step that fails.
pub fn sign(gpg: &dyn Gpg, r: &Request, output: Option<&Path>) -> Result<Vec<String>, Error> {
    let fingerprints = split_fingerprints(r.fingerprints)?;
    let release = r.dir.join("Release");
    if !release.is_file() {
        return Err(Error::Failed(format!(
            "{} does not exist: nothing to sign",
            release.display()
        )));
    }
    let home = r.work.join("gnupg");
    let made =
        private_dir(r.work, &home).map_err(Error::spawn(format!("create {}", home.display())));
    let done = made.and_then(|()| gpg_steps(gpg, &home, &fingerprints, r));
    // The key never outlives the verb. A runner is discarded after its job all the same, so a
    // file that is gone already is not a failure.
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_file(r.key_file);
    let _ = std::fs::remove_file(r.passphrase_file);
    done?;
    if let Some(output) = output {
        crate::release::append_output(output, &["signed=true".to_owned()])?;
    }
    Ok(vec![format!(
        "signed Release with {} subkey(s), {}: InRelease and Release.gpg verify",
        fingerprints.len(),
        fingerprints.join(", ")
    )])
}

#[cfg(test)]
mod tests;
