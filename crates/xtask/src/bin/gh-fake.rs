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
//! attestation is checked online only (and is what a verification without a bundle finds: a
//! line that names a bundle is not found without it). The verification must deny self-hosted
//! runners and name the branch `refs/heads/main` (or the one `expect.txt` names). Without
//! `--predicate-type` the predicate is build provenance.
//!
//! The command line is held to what the verb is to pass by `expect.txt` (beside the ledger),
//! lines `key=value`; a key that is not there is not checked:
//!
//! - `repo`: `--repo` is this;
//! - `source-ref`: `--source-ref` is this (default `refs/heads/main`);
//! - `issuer`: a verification with `--bundle` has `--cert-oidc-issuer` this;
//! - `root`: a verification with `--bundle` has `--custom-trusted-root` this path, or none
//!   when the value is `-`;
//! - `proxy`: a verification with `--bundle` runs with all six proxy variables set to this and
//!   neither bypass list (`NO_PROXY`, `no_proxy`) in its environment;
//! - `record`: not a check: every verification appends its arguments to the file this names,
//!   one per line, with an empty line after them, so a test can read what the verb passed.
//!
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
use std::io::{Read as _, Write as _};
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

/// The file `name` in the directory of `file` or the one above it, or nothing.
fn find_beside(file: &str, name: &str) -> String {
    Path::new(file)
        .ancestors()
        .skip(1)
        .take(2)
        .find_map(|dir| std::fs::read_to_string(dir.join(name)).ok())
        .unwrap_or_default()
}

/// The proxy variables a verification with a trusted root runs under.
const PROXIES: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// How the command line departs from what `expect.txt` says, if it does.
fn departure(expect: &str, flags: &BTreeMap<&str, &str>) -> Option<String> {
    let want: BTreeMap<&str, &str> = expect.lines().filter_map(|l| l.split_once('=')).collect();
    let flag = |name: &str| flags.get(name).copied();
    let source_ref = want.get("source-ref").copied().unwrap_or("refs/heads/main");
    if flag("--source-ref") != Some(source_ref) || flag("--deny-self-hosted-runners").is_none() {
        return Some("wrong --source-ref or no --deny-self-hosted-runners".to_owned());
    }
    if want.get("repo").is_some_and(|r| flag("--repo") != Some(r)) {
        return Some("wrong --repo".to_owned());
    }
    // The rest holds a verification of a bundle only.
    flag("--bundle")?;
    if want
        .get("issuer")
        .is_some_and(|i| flag("--cert-oidc-issuer") != Some(i))
    {
        return Some("wrong --cert-oidc-issuer".to_owned());
    }
    if let Some(root) = want.get("root") {
        let wanted = (*root != "-").then_some(*root);
        if flag("--custom-trusted-root") != wanted {
            return Some("wrong --custom-trusted-root".to_owned());
        }
    }
    if let Some(proxy) = want.get("proxy") {
        let dead = PROXIES
            .iter()
            .any(|v| std::env::var(v).ok().as_deref() != Some(*proxy));
        let bypass = ["NO_PROXY", "no_proxy"]
            .iter()
            .any(|v| std::env::var_os(v).is_some());
        if dead || bypass {
            return Some("wrong proxy environment".to_owned());
        }
    }
    None
}

/// Appends `args` to the file that the `record` line of `expect.txt` names, if there is one:
/// one argument per line, an empty line after them, in one write.
fn record(expect: &str, args: &[String]) {
    let Some(path) = expect.lines().find_map(|l| l.strip_prefix("record=")) else {
        return;
    };
    let block: String = args.iter().map(|a| format!("{a}\n")).collect();
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(format!("{block}\n").as_bytes()))
        .expect("gh-fake: the record file");
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
    let ledger = find_beside(&args[2], "ledger.txt");
    let expect = find_beside(&args[2], "expect.txt");
    record(&expect, args);
    if let Some(problem) = departure(&expect, &flags) {
        eprintln!("gh-fake: {problem}: {flags:?}");
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
                (None, None) => *b == "-",
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
