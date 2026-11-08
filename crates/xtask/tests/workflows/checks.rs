//! The rules for the jobs of `release.yml` that sit between the publication to testing and the
//! promotion (docs/design/distribution.md, section 5): `formula`, the three install checks
//! (`check-apt`, `check-brew`, `check-install-sh`) and `promote-check`. [`check_problems`] is
//! called from `release_problems`, so the one lint holds the whole file.
//!
//! - The jobs hold `contents: read` alone, name no environment, no secret, no variable and no
//!   token: an install check installs what is public and needs nothing private.
//! - `check-apt` runs in a clean Ubuntu container whose image is pinned by its digest, on a hosted
//!   runner of each architecture (amd64 and arm64), and runs `channels check-apt` with the
//!   architecture, the version and the commit of the release; it waits for every attempt of the
//!   apt write of testing, and runs only when one of them wrote.
//! - `check-brew` runs on the hosted `macos-15` runner, from the artifact `formula`; it and
//!   `check-install-sh` are the only jobs of `release.yml` on macOS. `check-install-sh` runs on
//!   Linux and on macOS and waits for `testing-upload`.
//! - Every job after a job that may be skipped has the `if:` that lets a skipped predecessor pass
//!   and stops at a failed one.
//! - `promote-check` waits for every job whose result it reads, is given each result as
//!   `NAME=$VAR` with `VAR` the `needs.<job>.result` of that job, and hands on `promote`.
//! - A container image anywhere in `release.yml` is pinned by digest.
//! - The verbs of these jobs are run by them alone.

use super::channels::{mentions, steps, text};
use super::hostkeeper::at;
use super::lint::{AFTER_IF, RELEASE_IF, entries, entry, names};
use super::yaml::Node;

/// The `if:` of `check-apt`: a write of testing wrote (`conflict=false` is what a write that
/// landed appends), after the attempts that were needed.
pub const APT_IF: &str = "${{ !cancelled() && !failure() && needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && (needs.testing-apt-write.outputs.conflict == 'false' || needs.testing-apt-write-2.outputs.conflict == 'false' || needs.testing-apt-write-3.outputs.conflict == 'false') }}";

/// The `if:` of `check-install-sh`: the files are at tierstore.dev/releases/<version>/.
pub const INSTALL_SH_IF: &str = "${{ !cancelled() && !failure() && needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.testing-upload.outputs.enabled == 'true' }}";

/// The results `promote-check` reads: the stage and the job whose result it is.
const STAGES: [(&str, &str); 10] = [
    ("testing-upload", "TESTING_UPLOAD"),
    ("testing-apt-write", "TESTING_APT_WRITE"),
    ("testing-apt-write-2", "TESTING_APT_WRITE_2"),
    ("testing-apt-write-3", "TESTING_APT_WRITE_3"),
    ("check-apt", "CHECK_APT"),
    ("check-brew", "CHECK_BREW"),
    ("check-install-sh", "CHECK_INSTALL_SH"),
    ("farm-check", "FARM_CHECK"),
    ("farm", "FARM"),
    ("soak", "SOAK"),
];

/// The jobs and the verbs only they run.
const VERBS: [(&str, &str); 6] = [
    ("formula", "cargo xtask channels formula "),
    ("check-apt", "cargo xtask channels check-apt "),
    ("check-brew", "cargo xtask channels check-brew "),
    ("check-install-sh", "cargo xtask channels check-install-sh "),
    ("promote-check", "cargo xtask channels decide "),
    ("farm-check", "cargo xtask release desired-deb "),
];

/// What a job is held to.
struct Want {
    name: &'static str,
    needs: &'static [&'static str],
    condition: &'static str,
}

const JOBS: [Want; 5] = [
    Want {
        name: "formula",
        needs: &["plan", "publish"],
        condition: RELEASE_IF,
    },
    Want {
        name: "check-apt",
        needs: &[
            "plan",
            "testing-apt-write",
            "testing-apt-write-2",
            "testing-apt-write-3",
        ],
        condition: APT_IF,
    },
    Want {
        name: "check-brew",
        needs: &["formula", "plan"],
        condition: AFTER_IF,
    },
    Want {
        name: "check-install-sh",
        needs: &["plan", "testing-upload"],
        condition: INSTALL_SH_IF,
    },
    Want {
        name: "promote-check",
        needs: &[
            "check-apt",
            "check-brew",
            "check-install-sh",
            "farm",
            "farm-check",
            "plan",
            "soak",
            "testing-apt-write",
            "testing-apt-write-2",
            "testing-apt-write-3",
            "testing-upload",
        ],
        condition: AFTER_IF,
    },
];

/// Is `word` a digest-pinned image reference: `@sha256:` and 64 hex digits?
fn pinned(word: &str) -> bool {
    word.split_once("@sha256:")
        .is_some_and(|(_, d)| d.len() >= 64 && d[..64].bytes().all(|b| b.is_ascii_hexdigit()))
}

fn permissions(job: &Node) -> Vec<String> {
    let mut held: Vec<String> = entry(job, "permissions")
        .map_or(&[][..], entries)
        .iter()
        .map(|(k, v)| {
            format!(
                "{}: {}",
                k.as_str().unwrap_or_default(),
                v.as_str().unwrap_or_default()
            )
        })
        .collect();
    held.sort();
    held
}

/// One line per way the check jobs of the `release.yml` document `doc` are not as the module says.
pub fn check_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut problems = Vec::new();
    for want in &JOBS {
        match jobs.iter().find(|(k, _)| k.as_str() == Some(want.name)) {
            Some((_, found)) => problems.extend(job_problems(want, found)),
            None => problems.push(format!("{}: there is no such job", want.name)),
        }
    }
    for (name, found) in jobs {
        let name = name.as_str().unwrap_or_default();
        for (owner, verb) in VERBS {
            if name != owner && steps(found).iter().any(|s| text(s, "run").contains(verb)) {
                problems.push(format!(
                    "{name}: runs `{}...`, which only {owner} may",
                    verb.trim_end()
                ));
            }
        }
        if entry(found, "runs-on").is_some_and(|r| mentions(r, "macos"))
            && !["check-brew", "check-install-sh"].contains(&name)
        {
            problems.push(format!(
                "{name}: runs on macOS; only check-brew and check-install-sh may"
            ));
        }
        if name != "check-apt"
            && steps(found)
                .iter()
                .any(|s| text(s, "run").contains("docker "))
        {
            problems.push(format!("{name}: runs docker; only check-apt may"));
        }
        if let Some(container) = entry(found, "container") {
            let image = container.as_str().or_else(|| at(container, &["image"]));
            if !image.is_some_and(pinned) {
                problems.push(format!(
                    "{name}: its container image is not pinned by digest (image@sha256:<64 hex digits>)"
                ));
            }
        }
    }
    problems
}

fn job_problems(want: &Want, job: &Node) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    if names(entry(job, "needs")) != want.needs {
        problems.push(format!("{name}: needs are not {:?}", want.needs));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(want.condition) {
        problems.push(format!("{name}: `if` is not {}", want.condition));
    }
    if entry(job, "environment").is_some() {
        problems.push(format!(
            "{name}: names an environment; it needs no setting and no secret"
        ));
    }
    if permissions(job) != ["contents: read"] {
        problems.push(format!(
            "{name}: permissions are {:?}, not contents: read alone",
            permissions(job)
        ));
    }
    for word in ["secrets.", "vars.", "id-token", "github.token", "GH_TOKEN"] {
        if mentions(job, word) {
            problems.push(format!("{name}: names {word}; this job holds no secret"));
        }
    }
    let all = steps(job);
    match name {
        "formula" => problems.extend(formula_problems(all)),
        "check-apt" => problems.extend(apt_problems(job, all)),
        "check-brew" => problems.extend(brew_problems(job, all)),
        "check-install-sh" => problems.extend(install_sh_problems(job, all)),
        _ => problems.extend(promote_problems(job, all)),
    }
    problems
}

fn runs_with<'a>(all: &'a [Node], verb: &str) -> Vec<&'a str> {
    all.iter()
        .map(|s| text(s, "run"))
        .filter(|r| r.contains(verb))
        .collect()
}

fn formula_problems(all: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    if runs_with(all, "cargo xtask release check dist").is_empty() {
        problems.push("formula: no step checks the downloaded files".to_owned());
    }
    if !runs_with(all, "cargo xtask channels formula ")
        .iter()
        .any(|r| r.contains("--sums dist/SHA256SUMS "))
    {
        problems.push(
            "formula: no step renders the formula from the verified dist/SHA256SUMS".to_owned(),
        );
    }
    let kept = all.iter().any(|s| {
        super::channels::uses(s, "actions/upload-artifact")
            && at(s, &["with", "name"]) == Some("formula")
    });
    if !kept {
        problems.push("formula: the formula is not kept as the artifact `formula`".to_owned());
    }
    problems
}

fn apt_problems(job: &Node, all: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    if at(job, &["runs-on"]) != Some("${{ matrix.runner }}") {
        problems.push("check-apt: runs-on is not ${{ matrix.runner }}".to_owned());
    }
    let strategy = entry(job, "strategy");
    for runner in ["ubuntu-24.04", "ubuntu-24.04-arm"] {
        if !strategy.is_some_and(|s| entry(s, "matrix").is_some_and(|m| mentions(m, runner))) {
            problems.push(format!(
                "check-apt: the matrix does not run on {runner}: both architectures are checked"
            ));
        }
    }
    for arch in ["amd64", "arm64"] {
        if !strategy.is_some_and(|s| mentions(s, arch)) {
            problems.push(format!("check-apt: the matrix has no {arch}"));
        }
    }
    let containers = runs_with(all, "docker run");
    let [run] = containers.as_slice() else {
        problems.push("check-apt: it needs one step that runs `docker run`".to_owned());
        return problems;
    };
    if !run
        .split_whitespace()
        .any(|w| w.starts_with("ubuntu@") && pinned(w))
    {
        problems.push(
            "check-apt: the container image is not ubuntu@sha256:<digest>, pinned by digest"
                .to_owned(),
        );
    }
    if !run.contains(":ro") {
        problems.push("check-apt: the xtask binary is not mounted read-only (:ro)".to_owned());
    }
    for argument in [
        "channels check-apt --arch \"$ARCH\" ",
        "--version \"${TAG#v}\" ",
        "--commit \"$COMMIT\"",
    ] {
        if !run.contains(argument) {
            problems.push(format!("check-apt: the check is not given {argument}"));
        }
    }
    problems
}

fn brew_problems(job: &Node, all: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    if at(job, &["runs-on"]) != Some("macos-15") {
        problems.push("check-brew: runs-on is not macos-15, the hosted macOS runner".to_owned());
    }
    let downloads = all.iter().any(|s| {
        super::channels::uses(s, "actions/download-artifact")
            && at(s, &["with", "name"]) == Some("formula")
    });
    if !downloads {
        problems.push("check-brew: no step downloads the artifact `formula`".to_owned());
    }
    if !runs_with(all, "cargo xtask channels check-brew ")
        .iter()
        .any(|r| r.contains("--version \"${TAG#v}\" ") && r.contains("--commit \"$COMMIT\""))
    {
        problems.push(
            "check-brew: no step runs `channels check-brew` with the version and the commit"
                .to_owned(),
        );
    }
    problems
}

fn install_sh_problems(job: &Node, all: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    if at(job, &["runs-on"]) != Some("${{ matrix.runner }}") {
        problems.push("check-install-sh: runs-on is not ${{ matrix.runner }}".to_owned());
    }
    for runner in ["ubuntu-24.04", "macos-15"] {
        if !entry(job, "strategy")
            .and_then(|s| entry(s, "matrix"))
            .is_some_and(|m| mentions(m, runner))
        {
            problems.push(format!(
                "check-install-sh: the matrix does not run on {runner}: Linux and macOS are both checked"
            ));
        }
    }
    if !runs_with(all, "cargo xtask channels check-install-sh ")
        .iter()
        .any(|r| r.contains("--version \"${TAG#v}\" ") && r.contains("--commit \"$COMMIT\""))
    {
        problems.push(
            "check-install-sh: no step runs `channels check-install-sh` with the version and the commit"
                .to_owned(),
        );
    }
    problems
}

fn promote_problems(job: &Node, all: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    if at(job, &["outputs", "promote"]) != Some("${{ steps.decide.outputs.promote }}") {
        problems.push(
            "promote-check: the output promote is not ${{ steps.decide.outputs.promote }}"
                .to_owned(),
        );
    }
    let Some(step) = all.iter().find(|s| at(s, &["id"]) == Some("decide")) else {
        problems.push("promote-check: no step has the id `decide`".to_owned());
        return problems;
    };
    let run = text(step, "run");
    if !run.starts_with("cargo xtask channels decide ") {
        problems.push(
            "promote-check: the step `decide` does not run `cargo xtask channels decide`"
                .to_owned(),
        );
    }
    for (stage, var) in STAGES {
        if !run.contains(&format!("--result \"{stage}=${var}\"")) {
            problems.push(format!(
                "promote-check: the decision is not given --result \"{stage}=${var}\""
            ));
        }
        let want = format!("${{{{ needs.{stage}.result }}}}");
        if at(step, &["env", var]) != Some(want.as_str()) {
            problems.push(format!(
                "promote-check: env {var} is not {want}, the result of {stage}"
            ));
        }
    }
    if run.matches("--result ").count() != STAGES.len() {
        problems.push("promote-check: the decision is given a result it should not be".to_owned());
    }
    if !run.contains("--farm-enabled \"$FARM_ENABLED\"")
        || at(step, &["env", "FARM_ENABLED"]) != Some("${{ needs.farm-check.outputs.enabled }}")
    {
        problems.push(
            "promote-check: the decision is not given the lab's setting (FARM_ENABLED from needs.farm-check.outputs.enabled)"
                .to_owned(),
        );
    }
    if !run.contains("--output \"$GITHUB_OUTPUT\"") {
        problems.push("promote-check: the decision is not written to GITHUB_OUTPUT".to_owned());
    }
    problems
}
