//! `channels rpm sign` and `channels rpm check-signed`: the signature of the packages.
//!
//! `sign` puts into each package of the release the header-only OpenPGP signature of the archive
//! signing subkey, and nothing else changes: the lead, the main header and the payload are the
//! unsigned package's, and the signature header gets one entry ([`layout::with_signature`]). So
//! the unsigned package, which the build attested and `SHA256SUMS` lists, is the signed one with
//! that entry taken out, and `check-signed` proves it for every package. Each package is first held
//! to its digest in `SHA256SUMS`, then signed with `gpg` (the key spec is the subkey's fingerprint
//! and `!`, the digest SHA-256, the pinentry loopback with the passphrase in a file, as `apt sign`
//! does), then the signature is verified with `gpg` against the header and read back to see that
//! the key that made it is the one asked for. The signature's time is `--signed-at` (the job passes
//! the commit's), frozen with gpg's `--faked-system-time`, so a re-run, and the promotion's own
//! run, give the same bytes for the same key: RSA and EdDSA signatures are deterministic. When
//! several fingerprints are given (a key rotation), the first signs the packages; the repository's
//! `repomd.xml.asc` has a signature of each.
//!
//! The signed packages go to `--out`, with `SHA256SUMS.signed`, their own digests: the index lists
//! those, and the promotion holds `testing`'s entries to them.

use std::path::Path;

use super::layout::{self, signer, tag_of};
use crate::Error;
use crate::channels::apt::sign::{Gpg, args, private_dir, split_fingerprints};
use crate::fetch::Sha256;
use crate::package::{read, write_new};
use crate::release::rpm::inspect;
use crate::sums::{self, Line};

/// The digests of the signed packages, in the form of `SHA256SUMS`.
pub const SIGNED_SUMS: &str = "SHA256SUMS.signed";

/// What `rpm sign` is asked.
pub struct Request<'a> {
    pub rpms: &'a Path,
    pub out: &'a Path,
    pub version: &'a str,
    pub sums: &'a Path,
    pub work: &'a Path,
    pub fingerprints: &'a str,
    pub key_file: &'a Path,
    pub passphrase_file: &'a Path,
    pub signed_at: u64,
}

/// The line of `lines` for `name`, and the unsigned bytes' agreement with it.
fn attested(lines: &[Line], name: &str, unsigned: &[u8]) -> Result<(), Error> {
    let line = lines
        .iter()
        .find(|l| l.name == name)
        .ok_or_else(|| Error::Failed(format!("{} lists no {name}", sums::SUMS)))?;
    let digest = Sha256::of(unsigned);
    if line.sha256 == digest {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "{name} has sha256 {digest}, but {} lists {}: it is not the package that was attested",
            sums::SUMS,
            line.sha256
        )))
    }
}

/// The `rpm sign` verb: the lines to print.
///
/// # Errors
/// A fingerprint or version that is not one, a package that is missing or is not the attested one
/// (or is signed already), a private directory that cannot be made, or a gpg step that fails or a
/// signature that is not by the key asked for.
pub fn sign(gpg: &dyn Gpg, r: &Request, output: Option<&Path>) -> Result<Vec<String>, Error> {
    let done = sign_files(gpg, r);
    // The key never outlives the verb, whatever it came to; a file that is gone already is not a
    // failure.
    let _ = std::fs::remove_dir_all(r.work.join("gnupg"));
    let _ = std::fs::remove_file(r.key_file);
    let _ = std::fs::remove_file(r.passphrase_file);
    let said = done?;
    if let Some(output) = output {
        crate::release::append_output(output, &["signed=true".to_owned()])?;
    }
    Ok(said)
}

fn sign_files(gpg: &dyn Gpg, r: &Request) -> Result<Vec<String>, Error> {
    let fingerprints = split_fingerprints(r.fingerprints)?;
    let fpr = &fingerprints[0];
    let lines = sums::read_sums(r.sums)?;
    let mut unsigned = Vec::new();
    for (name, path) in super::package_files(r.rpms, r.version)? {
        let bytes = read(&path)?;
        attested(&lines, &name, &bytes)?;
        unsigned.push((name, bytes));
    }
    let home = r.work.join("gnupg");
    private_dir(r.work, &home).map_err(Error::spawn(format!("create {}", home.display())))?;
    let text = |p: &Path| p.display().to_string();
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
    run(
        &format!("the signing key is not among the secret keys that were imported ({fpr})"),
        base(&["--list-secret-keys", "--with-colons", fpr]),
    )?;
    let (header_file, signature_file) = (r.work.join("header.bin"), r.work.join("header.sig"));
    std::fs::create_dir_all(r.out).map_err(Error::spawn(format!("create {}", r.out.display())))?;
    let mut signed_lines = Vec::new();
    let mut said = Vec::new();
    for (name, bytes) in &unsigned {
        std::fs::write(&header_file, layout::split(bytes)?.header)
            .map_err(Error::spawn(format!("write {}", header_file.display())))?;
        run(
            &format!("gpg could not sign {name}"),
            base(&[
                "--pinentry-mode",
                "loopback",
                "--passphrase-file",
                &text(r.passphrase_file),
                "--digest-algo",
                "SHA256",
                "--faked-system-time",
                &format!("{}!", r.signed_at),
                "--local-user",
                &format!("{fpr}!"),
                "--no-armor",
                "--detach-sign",
                "--output",
                &text(&signature_file),
                &text(&header_file),
            ]),
        )?;
        run(
            &format!("the signature of {name} does not verify"),
            base(&["--verify", &text(&signature_file), &text(&header_file)]),
        )?;
        let signature = read(&signature_file)?;
        let by = signer(&signature)?;
        if !fpr.ends_with(&by.key_id) {
            return Err(Error::Failed(format!(
                "{name} was signed by the key {}, not by {fpr}",
                by.key_id
            )));
        }
        let signed = layout::with_signature(bytes, tag_of(by.algorithm)?, &signature)?;
        write_new(&r.out.join(name), &signed)?;
        said.push(format!("signed {name} with {}", by.key_id));
        signed_lines.push(Line {
            sha256: Sha256::of(&signed),
            name: name.clone(),
        });
    }
    let path = r.out.join(SIGNED_SUMS);
    std::fs::write(&path, sums::render(&signed_lines))
        .map_err(Error::spawn(format!("write {}", path.display())))?;
    said.push(format!("{SIGNED_SUMS}: {} packages", signed_lines.len()));
    Ok(said)
}

/// What `rpm check-signed` is asked.
pub struct Check<'a> {
    pub dir: &'a Path,
    pub version: &'a str,
    pub sums: &'a Path,
    /// The `SHA256SUMS.signed` `rpm sign` wrote, when the digests of the signed files are held to it.
    pub signed_sums: Option<&'a Path>,
    /// The fingerprint the signature must be by, when it is held to one.
    pub fingerprint: Option<&'a str>,
}

/// The `rpm check-signed` verb: the lines to print.
///
/// # Errors
/// A package that is missing, is not signed, is signed by another key or with a tag its algorithm
/// does not have, is damaged (its digests or its payload), is not the attested package once its
/// signature is taken out, or is not the file `SHA256SUMS.signed` lists.
pub fn check_signed(r: &Check) -> Result<Vec<String>, Error> {
    let lines = sums::read_sums(r.sums)?;
    let signed_lines = r.signed_sums.map(sums::read_sums).transpose()?;
    let fingerprint = r
        .fingerprint
        .map(|f| split_fingerprints(f).map(|l| l[0].clone()))
        .transpose()?;
    let mut said = Vec::new();
    for (name, path) in super::package_files(r.dir, r.version)? {
        let bytes = read(&path)?;
        let failed = |why: String| Error::Failed(format!("{name}: {why}"));
        let parts = layout::split(&bytes)?;
        let (tag, signature) = parts
            .signature()
            .ok_or_else(|| failed("it is not signed".to_owned()))?;
        let by = signer(signature)?;
        if tag != tag_of(by.algorithm)? {
            return Err(failed(format!(
                "its signature is filed under the tag {tag}, which its algorithm does not use"
            )));
        }
        if let Some(fpr) = &fingerprint
            && !fpr.ends_with(&by.key_id)
        {
            return Err(failed(format!(
                "it is signed by the key {}, not by {fpr}",
                by.key_id
            )));
        }
        inspect(&bytes)?;
        attested(&lines, &name, &layout::without_signatures(&bytes)?)
            .map_err(|e| failed(format!("with its signature taken out, {e}")))?;
        if let Some(listed) = &signed_lines {
            let digest = Sha256::of(&bytes);
            if !listed.iter().any(|l| l.name == name && l.sha256 == digest) {
                return Err(failed(format!(
                    "its sha256 {digest} is not the one {SIGNED_SUMS} lists"
                )));
            }
        }
        said.push(format!(
            "{name}: signed by {}; without the signature it is the attested package",
            by.key_id
        ));
    }
    Ok(said)
}

#[cfg(test)]
mod tests;
