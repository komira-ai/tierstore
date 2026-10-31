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

//! The other things the tests ask of a `gh` (or of a `cargo`), all driven by files:
//!
//! - `--version` prints `gh-fake 0.1.0+0123456789ab` (the form of `tierstore --version`);
//! - `api [-X <method>] <path> [--input -]` answers with the file `<path>` when there is one
//!   (an empty file is an empty reply), with `(HTTP 404)` when `<path>.404` is there, with a
//!   failure when `<path>.fail` is, and with a failure otherwise; a body on stdin
//!   (`--input -`) is saved as `<path>.body`;
//! - `deny ...` (the `cargo deny` of the advisories verb) exits with the number in `deny.code`
//!   of the working directory (0 when there is none) and prints `deny.stderr` on stderr;
//! - `print <text>` and `fail <text>` print on stdout and exit 0, and print on stderr and
//!   exit 1.

use std::collections::BTreeMap;
use std::io::Read as _;
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
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!("gh-fake 0.1.0+0123456789ab");
            ExitCode::SUCCESS
        }
        Some("api") => api(&args[1..]),
        Some("deny") => deny(),
        Some("print") => {
            println!("{}", args[1..].join(" "));
            ExitCode::SUCCESS
        }
        Some("fail") => {
            eprintln!("{}", args[1..].join(" "));
            ExitCode::FAILURE
        }
        _ => attestation(&args),
    }
}

/// `api [-X <method>] <path> [--input -]`: see the header.
fn api(args: &[String]) -> ExitCode {
    let mut path = None;
    let mut body = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-X" => {
                rest.next();
            }
            "--input" => {
                rest.next();
                body = true;
            }
            _ => path = Some(arg.as_str()),
        }
    }
    let Some(path) = path else {
        eprintln!("gh-fake: api needs a path");
        return ExitCode::from(2);
    };
    if body {
        let mut text = String::new();
        let _ = std::io::stdin().read_to_string(&mut text);
        let _ = std::fs::write(format!("{path}.body"), text);
    }
    if let Ok(reply) = std::fs::read(path) {
        let _ = std::io::Write::write_all(&mut std::io::stdout(), &reply);
        return ExitCode::SUCCESS;
    }
    if Path::new(&format!("{path}.404")).exists() {
        eprintln!("gh: Not Found (HTTP 404)");
    } else if Path::new(&format!("{path}.fail")).exists() {
        eprintln!("gh: the call failed");
    } else {
        eprintln!("gh-fake: no reply is prepared for {path}");
    }
    ExitCode::FAILURE
}

/// `deny ...`: see the header.
fn deny() -> ExitCode {
    let code = std::fs::read_to_string("deny.code")
        .ok()
        .and_then(|text| text.trim().parse::<u8>().ok())
        .unwrap_or(0);
    eprint!(
        "{}",
        std::fs::read_to_string("deny.stderr").unwrap_or_default()
    );
    ExitCode::from(code)
}

/// `attestation verify <file> ...`: see the header.
fn attestation(args: &[String]) -> ExitCode {
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
