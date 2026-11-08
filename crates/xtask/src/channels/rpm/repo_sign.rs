//! `channels rpm repo sign --dir <dir> --work <dir> --fingerprint <fpr>[,<fpr>...] --key-file <file>
//! --passphrase-file <file> [--gpg <program>] [--output <file>]`: `repomd.xml.asc`, the detached
//! armored signature of each architecture's `repomd.xml`, verified.
//!
//! `dir` is `<next>/<suite>`, what `rpm repo index` or `rpm promote` wrote. The key is the signing
//! subkey of the archive key, selected as `apt sign` selects it (the fingerprint and `!`); several
//! fingerprints (a key rotation) give one signature each. With `repo_gpgcheck=1`, `dnf` verifies this
//! file, and `repomd.xml` then vouches for every metadata file. The passphrase is a file, with the
//! loopback pinentry; the key, the passphrase and the keyring are removed when the verb ends,
//! whatever happened. `signed=true` is appended to `--output` when every signature verifies.

use std::path::Path;

use super::state::{ARCHS, REPOMD, REPOMD_ASC};
use crate::Error;
use crate::channels::apt::sign::{Gpg, args, private_dir, split_fingerprints};

/// What `rpm repo sign` is asked.
pub struct Request<'a> {
    pub dir: &'a Path,
    pub work: &'a Path,
    pub fingerprints: &'a str,
    pub key_file: &'a Path,
    pub passphrase_file: &'a Path,
}

/// The `rpm repo sign` verb: the lines to print.
///
/// # Errors
/// A fingerprint that is not one, a `repomd.xml` that is not there, a private directory that cannot
/// be made, or a gpg step that fails.
pub fn sign(gpg: &dyn Gpg, r: &Request, output: Option<&Path>) -> Result<Vec<String>, Error> {
    let done = sign_files(gpg, r);
    // The key never outlives the verb, whatever it came to; a file that is gone already is not a
    // failure.
    let _ = std::fs::remove_dir_all(r.work.join("gnupg"));
    let _ = std::fs::remove_file(r.key_file);
    let _ = std::fs::remove_file(r.passphrase_file);
    let fingerprints = done?;
    if let Some(output) = output {
        crate::release::append_output(output, &["signed=true".to_owned()])?;
    }
    Ok(vec![format!(
        "signed repomd.xml of {} architectures with {} subkey(s), {}: {REPOMD_ASC} verifies",
        ARCHS.len(),
        fingerprints.len(),
        fingerprints.join(", ")
    )])
}

fn sign_files(gpg: &dyn Gpg, r: &Request) -> Result<Vec<String>, Error> {
    let fingerprints = split_fingerprints(r.fingerprints)?;
    let text = |p: &Path| p.display().to_string();
    let files: Vec<(String, String)> = ARCHS
        .iter()
        .map(|arch| {
            let dir = r.dir.join(arch).join("repodata");
            (text(&dir.join(REPOMD)), text(&dir.join(REPOMD_ASC)))
        })
        .collect();
    if let Some((repomd, _)) = files
        .iter()
        .find(|(repomd, _)| !Path::new(repomd).is_file())
    {
        return Err(Error::Failed(format!(
            "{repomd} does not exist: nothing to sign"
        )));
    }
    let home = r.work.join("gnupg");
    private_dir(r.work, &home).map_err(Error::spawn(format!("create {}", home.display())))?;
    let base = |extra: &[&str]| {
        let mut v = args(&["--homedir", &text(&home), "--batch", "--no-tty", "--yes"]);
        v.extend(args(extra));
        v
    };
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
    for fpr in &fingerprints {
        run(
            &format!("the signing key is not among the secret keys that were imported ({fpr})"),
            base(&["--list-secret-keys", "--with-colons", fpr]),
        )?;
    }
    for (repomd, asc) in &files {
        let mut v = base(&[
            "--pinentry-mode",
            "loopback",
            "--passphrase-file",
            &text(r.passphrase_file),
            "--digest-algo",
            "SHA512",
        ]);
        for fpr in &fingerprints {
            v.extend(args(&["--local-user", &format!("{fpr}!")]));
        }
        v.extend(args(&["--armor", "--detach-sign", "--output", asc, repomd]));
        run(&format!("gpg could not sign {repomd}"), v)?;
        run(
            &format!("{asc} does not verify"),
            base(&["--verify", asc, repomd]),
        )?;
    }
    Ok(fingerprints)
}

#[cfg(test)]
mod tests;
