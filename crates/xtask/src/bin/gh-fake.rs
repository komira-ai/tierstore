//! A TEST-ONLY stand-in for `gh attestation verify`, which `tests/attestations.rs` hands to
//! `verify-assets attestations` in place of the real program.
//!
//! It accepts a verification when a line of the ledger (`ledger.txt`, in the directory of the
//! file verified or the one above it) describes it, as a real attestation would: one line per
//! attestation, fields separated by `|`:
//!
//! ```text
//! <subject sha256>|<signer workflow>|<predicate type>|<bundle sha256>|<commit>
//! ```
//!
//! `*` in the subject, bundle or commit field matches anything (a deliberately sloppy
//! attestation, to see the controls of the verb refuse it); `-` in the bundle field means the
//! attestation is checked online only. The verification must name the branch `refs/heads/main`
//! and deny self-hosted runners. Without `--predicate-type` the predicate is build provenance.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use sha2::{Digest, Sha256};

const PROVENANCE: &str = "https://slsa.dev/provenance/v1";

fn digest(path: &str) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 || args[0] != "attestation" || args[1] != "verify" {
        eprintln!("gh-fake: only `attestation verify <file> ...` is understood: {args:?}");
        return ExitCode::from(2);
    }
    let mut flags: BTreeMap<&str, &str> = BTreeMap::new();
    let mut rest = args[3..].iter();
    while let Some(flag) = rest.next() {
        if flag == "--deny-self-hosted-runners" {
            flags.insert("--deny-self-hosted-runners", "");
        } else if let Some(value) = rest.next() {
            flags.insert(flag, value);
        }
    }
    let Some(subject) = digest(&args[2]) else {
        eprintln!("gh-fake: cannot read {}", args[2]);
        return ExitCode::FAILURE;
    };
    let bundle = flags.get("--bundle").and_then(|b| digest(b));
    let ledger = Path::new(&args[2])
        .ancestors()
        .skip(1)
        .take(2)
        .find_map(|dir| std::fs::read_to_string(dir.join("ledger.txt")).ok())
        .unwrap_or_default();
    if flags.get("--source-ref").copied() != Some("refs/heads/main")
        || !flags.contains_key("--deny-self-hosted-runners")
    {
        eprintln!("gh-fake: wrong --source-ref or no --deny-self-hosted-runners: {flags:?}");
        return ExitCode::FAILURE;
    }
    let signer = flags.get("--signer-workflow").copied().unwrap_or_default();
    let predicate = flags.get("--predicate-type").copied().unwrap_or(PROVENANCE);
    let commit = flags.get("--source-digest").copied();
    let accepted = ledger.lines().any(|line| {
        let f: Vec<&str> = line.split('|').collect();
        let [s, w, p, b, c] = f.as_slice() else {
            return false;
        };
        let same = |field: &str, got: &str| field == "*" || field == got;
        same(s, &subject)
            && *w == signer
            && *p == predicate
            && match (&bundle, commit) {
                (Some(bundle), Some(commit)) => same(b, bundle) && same(c, commit),
                (None, None) => true,
                _ => false,
            }
    });
    if accepted {
        ExitCode::SUCCESS
    } else {
        eprintln!("gh-fake: no attestation in the ledger matches {flags:?} for {subject}");
        ExitCode::FAILURE
    }
}
