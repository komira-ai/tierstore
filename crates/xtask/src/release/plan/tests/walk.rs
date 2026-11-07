//! The plan's walk of the first-parent history of the tip of `main`: which commit is released
//! when pushes were built under a version a release has since taken.

use super::*;

/// The highest release tag of the walk tests, and the commit it was published from.
const TAG: &str = "v0.3.55";

/// The full commit `n` (the tip of the walk tests is `COMMIT`).
fn sha(n: u64) -> String {
    format!("{n:040x}")
}

/// One commit of a history: its first parent, the version its build run recorded (none: no
/// record), and whether both its push runs succeeded.
struct Pushed {
    sha: String,
    parent: String,
    record: Option<&'static str>,
    green: bool,
}

fn pushed(sha: &str, parent: &str, record: Option<&'static str>, green: bool) -> Pushed {
    Pushed {
        sha: sha.to_owned(),
        parent: parent.to_owned(),
        record,
        green,
    }
}

fn runs_of(workflow: &str, commit: &str) -> String {
    format!(
        "repos/{REPO}/actions/workflows/{workflow}/runs?head_sha={commit}&event=push&branch=main&status=success&per_page=20"
    )
}

fn green_of(id: u64, commit: &str) -> Value {
    json!({ "workflow_runs": [{
        "id": id, "head_sha": commit, "head_branch": "main",
        "event": "push", "conclusion": "success",
    }]})
}

/// The build run id of the commit at `index` of a history.
fn run_id(index: usize) -> u64 {
    100 + index as u64
}

/// A repository whose highest release tag is [`TAG`] and whose `main` after it is `history`,
/// newest first (the first is the tip), each commit compared with the tag as `ahead` with
/// `files` changed and one title per commit up to it. `extra` commits (brought in by a merge)
/// are listed by the compare but have no runs.
fn history(history: &[Pushed], extra: &[&str], files: &[&str]) -> FakeGh {
    let mut api = vec![(tags_path(), refs(&["v0.3.54", TAG]))];
    let listed: Vec<Value> = history
        .iter()
        .map(|c| (c.sha.as_str(), c.parent.as_str()))
        .chain(extra.iter().map(|e| (*e, TAG)))
        .map(|(sha, parent)| {
            json!({
                "sha": sha,
                "parents": [{ "sha": parent }],
                "commit": { "message": format!("change {sha}") },
            })
        })
        .collect();
    let base = history.last().map_or(String::new(), |c| c.parent.clone());
    for (index, c) in history.iter().enumerate() {
        api.push((
            format!("repos/{REPO}/commits/{}", c.sha),
            json!({ "sha": c.sha, "parents": [{ "sha": c.parent }] }),
        ));
        let compared = json!({
            "status": "ahead",
            "merge_base_commit": { "sha": base },
            "files": files.iter().map(|f| json!({ "filename": f })).collect::<Vec<_>>(),
            "commits": listed[index..].iter().rev().cloned().collect::<Vec<_>>(),
        });
        api.push((format!("repos/{REPO}/compare/{TAG}...{}", c.sha), compared));
        if c.green {
            api.push((
                runs_of("artifacts.yml", &c.sha),
                green_of(run_id(index), &c.sha),
            ));
            api.push((runs_of("ci.yml", &c.sha), green_of(9, &c.sha)));
        }
        if let Some(version) = c.record {
            api.push((artifacts_path(run_id(index)), recorded(&[version])));
        }
    }
    FakeGh {
        api,
        ..FakeGh::default()
    }
}

/// The plan of the tip `COMMIT`, and what it appended to the output file.
fn plan_tip(gh: &FakeGh, name: &str) -> (Vec<String>, String) {
    let dir = scratch(name);
    let out = dir.join("out");
    let printed = plan(gh, &request(None, "", Some(&out))).expect("plan");
    let written = std::fs::read_to_string(&out).expect("out");
    (printed, written)
}

fn asked(gh: &FakeGh, commit: &str) -> bool {
    gh.asked.borrow().iter().any(|p| p.contains(commit))
}

#[test]
fn two_commits_built_as_one_version_after_one_is_released_the_other_is_passed_over() {
    // The wedge of v0.3.56: two pushes were built while v0.3.55 was the next version, so both
    // binaries say 0.3.55. The older (sha 1) was released as v0.3.55; the newer, COMMIT, is the
    // tip, and the next tag is v0.3.56, which its binary does not say.
    // Catches: the plan naming it v0.3.56, which verify's check-binary refuses on every run
    // until a newer push is built, and a red plan instead of a green one that says why.
    let gh = history(
        &[pushed(COMMIT, &sha(1), Some("0.3.55"), true)],
        &[],
        &["crates/tierstore/src/lib.rs"],
    );
    let (printed, written) = plan_tip(&gh, "walk-wedge");
    let reason = format!(
        "no commit of main is ready to be v0.3.56: {COMMIT}: built as 0.3.55, not v0.3.56; a newer commit's release carries it"
    );
    assert_eq!(written, format!("release=false\nreason={reason}\n"));
    assert_eq!(printed[0], format!("no release: {reason}"));
}

#[test]
fn the_newest_commit_built_as_the_next_tag_is_released_and_carries_the_one_passed_over() {
    // The push after the wedge: the tip was built as 0.3.56, its parent as 0.3.55.
    // Catches: the stale parent retried instead of superseded, the walk going on past a ready
    // commit, and the note leaving out the passed-over commit's change.
    let gh = history(
        &[
            pushed(COMMIT, &sha(2), Some("0.3.56"), true),
            pushed(&sha(2), &sha(1), Some("0.3.55"), true),
        ],
        &[],
        &["crates/tierstore/src/lib.rs"],
    );
    let (printed, written) = plan_tip(&gh, "walk-next");
    assert_eq!(printed[0], format!("release v0.3.56 of {COMMIT}"));
    assert_eq!(
        written,
        format!(
            "release=true\ntag=v0.3.56\ncommit={COMMIT}\nnote=Changes since v0.3.55: change {}; change {COMMIT}.\n",
            sha(2)
        )
    );
    assert!(!asked(&gh, &sha(2)), "{:?}", gh.asked.borrow());
}

#[test]
fn an_older_commit_built_as_the_next_tag_is_released_while_the_tip_is_not_ready() {
    // The tip's runs have not both succeeded (or it was built under another version); its
    // parent is ready as v0.3.56.
    // Catches: a plan that only looks at the tip, so a run that a newer one replaced in the
    // release group leaves a ready commit unreleased, and the note or the docs check taken from
    // the tip's compare instead of the released commit's.
    for tip in [
        pushed(COMMIT, &sha(2), None, false),
        pushed(COMMIT, &sha(2), Some("0.3.55"), true),
    ] {
        let gh = history(
            &[tip, pushed(&sha(2), &sha(1), Some("0.3.56"), true)],
            &[],
            &["crates/tierstore/src/lib.rs"],
        );
        let (printed, written) = plan_tip(&gh, "walk-older");
        assert_eq!(printed[0], format!("release v0.3.56 of {}", sha(2)));
        assert_eq!(
            written,
            format!(
                "release=true\ntag=v0.3.56\ncommit={}\nnote=Changes since v0.3.55: change {}.\n",
                sha(2),
                sha(2)
            )
        );
    }
}

#[test]
fn every_commit_passed_over_is_named_with_its_reason() {
    // Catches: a reason that names only the tip, and a commit with no runs, a red run, a run
    // without a version record and a run built under another version not told apart.
    let gh = history(
        &[
            pushed(COMMIT, &sha(4), None, false),
            pushed(&sha(4), &sha(3), None, true),
            pushed(&sha(3), &sha(2), Some("0.3.55"), true),
            pushed(&sha(2), &sha(1), Some("0.3.54"), true),
        ],
        &[],
        &["crates/tierstore/src/lib.rs"],
    );
    let (_, written) = plan_tip(&gh, "walk-reasons");
    assert_eq!(
        written,
        format!(
            "release=false\nreason=no commit of main is ready to be v0.3.56: \
             {COMMIT}: no successful push run of artifacts.yml on main yet; \
             {}: run 101 records no version-<X.Y.Z> artifact, so the version its binary says is not known; \
             {}: built as 0.3.55, not v0.3.56; a newer commit's release carries it; \
             {}: built as 0.3.54, not v0.3.56; a newer commit's release carries it\n",
            sha(4),
            sha(3),
            sha(2)
        )
    );
}

#[test]
fn a_commit_a_merge_brought_in_is_not_judged() {
    // A merge commit's second parent is listed by the compare but was never the tip of main.
    // Catches: a walk over every listed commit instead of the first-parent history, which asks
    // for runs that never exist and could release a branch commit.
    let branch = sha(7);
    let gh = history(
        &[
            pushed(COMMIT, &sha(2), Some("0.3.55"), true),
            pushed(&sha(2), &sha(1), Some("0.3.55"), true),
        ],
        &[&branch],
        &["crates/tierstore/src/lib.rs"],
    );
    let (printed, _) = plan_tip(&gh, "walk-merge");
    assert!(printed.contains(&"release=false".to_owned()), "{printed:?}");
    assert!(asked(&gh, &sha(2)), "{:?}", gh.asked.borrow());
    assert!(!asked(&gh, &branch), "{:?}", gh.asked.borrow());
}

#[test]
fn the_walk_judges_twenty_commits_and_says_it_left_older_ones() {
    // Catches: an unbounded walk (four API calls a commit, on every run), and a cut that is
    // not reported.
    let chain: Vec<String> = std::iter::once(COMMIT.to_owned())
        .chain((1..=24).rev().map(sha))
        .collect();
    let commits: Vec<Pushed> = chain
        .iter()
        .enumerate()
        .map(|(i, c)| pushed(c, chain.get(i + 1).map_or(&sha(0), |p| p), None, false))
        .collect();
    let gh = history(&commits, &[], &["crates/tierstore/src/lib.rs"]);
    let (_, written) = plan_tip(&gh, "walk-limit");
    assert!(
        written.ends_with(&format!(
            "{}: no successful push run of artifacts.yml on main yet; older commits not judged\n",
            chain[19]
        )),
        "{written}"
    );
    assert!(asked(&gh, &chain[19]), "{:?}", gh.asked.borrow());
    assert!(
        !gh.asked
            .borrow()
            .iter()
            .any(|p| p.contains("/runs?") && p.contains(&chain[20])),
        "{:?}",
        gh.asked.borrow()
    );
}

#[test]
fn a_ready_commit_whose_changes_are_docs_alone_is_not_released() {
    // Catches: the docs-only rule skipped for a commit below the tip.
    let gh = history(
        &[
            pushed(COMMIT, &sha(2), None, false),
            pushed(&sha(2), &sha(1), Some("0.3.56"), true),
        ],
        &[],
        &["docs/ci-and-release.md"],
    );
    let (printed, written) = plan_tip(&gh, "walk-docs");
    assert!(printed[0].contains("only docs"), "{printed:?}");
    assert!(written.starts_with("release=false\n"), "{written}");
}

#[test]
fn a_tag_that_appears_between_the_two_compares_covers_the_ready_commit() {
    // A release was published after the tip was compared: the ready commit's own compare says
    // the tag covers it.
    // Catches: a second tag cut for a commit a release already holds.
    let mut gh = history(
        &[
            pushed(COMMIT, &sha(2), None, false),
            pushed(&sha(2), &sha(1), Some("0.3.56"), true),
        ],
        &[],
        &["crates/tierstore/src/lib.rs"],
    );
    let path = format!("repos/{REPO}/compare/{TAG}...{}", sha(2));
    for (p, v) in &mut gh.api {
        if *p == path {
            v["status"] = json!("identical");
        }
    }
    let (_, written) = plan_tip(&gh, "walk-raced");
    assert_eq!(
        written,
        format!("release=false\nreason={TAG} already covers {}\n", sha(2))
    );
}

#[test]
fn a_rerun_build_is_judged_by_its_highest_version_record() {
    // A re-run of artifacts.yml computes the version again (never lower) and records it beside
    // the first attempt's.
    // Catches: the first attempt's record taken, so a commit rebuilt as the next tag stays
    // passed over; and a record that is not a version, or another artifact, read as one.
    let mut gh = repo(&[TAG], "ahead", &["Cargo.lock"], &["fix"]);
    gh.api.retain(|(p, _)| *p != artifacts_path(11));
    let mut listed = recorded(&["0.3.55", "0.3.56", "soon", "0.3.x"]);
    listed["artifacts"]
        .as_array_mut()
        .expect("artifacts")
        .push(json!({ "name": "versions-9.9.9" }));
    gh.api.push((artifacts_path(11), listed));
    let printed = plan_of(&gh, None);
    assert!(printed.contains(&"tag=v0.3.56".to_owned()), "{printed:?}");
}

#[test]
fn a_build_run_whose_artifacts_github_does_not_list_is_passed_over() {
    // Catches: a missing listing (404) taken for a match, or an error that turns the plan red.
    let mut gh = repo(&[TAG], "ahead", &["Cargo.lock"], &["fix"]);
    gh.api.retain(|(p, _)| *p != artifacts_path(11));
    let printed = plan_of(&gh, None);
    assert!(printed.contains(&"release=false".to_owned()), "{printed:?}");
    assert!(printed[0].contains("records no version-"), "{printed:?}");
}

#[test]
fn the_first_release_also_needs_its_build_to_record_its_tag() {
    // Catches: the repository without a tag skipping the version check.
    let gh = record_as(repo(&[], "ahead", &[], &[]), "0.0.9");
    let printed = plan_of(&gh, None);
    assert!(printed.contains(&"release=false".to_owned()), "{printed:?}");
    assert!(
        printed[0].contains("built as 0.0.9, not v0.1.0"),
        "{printed:?}"
    );
}

#[test]
fn the_version_is_recorded_in_the_file_the_build_uploads() {
    // Catches: `--record` not written, written in another form than the name the plan reads
    // (`X.Y.Z`, no `v`), and a write that fails silently.
    let dir = scratch("version-record");
    let record = dir.join("version");
    let gh = repo(&[TAG], "ahead", &[], &[]);
    let printed = version(&gh, REPO, None, None, Some(&record)).expect("version");
    assert_eq!(printed, vec!["version=0.3.56".to_owned()]);
    assert_eq!(
        std::fs::read_to_string(&record).expect("record"),
        "0.3.56\n"
    );
    let e = version(&gh, REPO, None, None, Some(&dir.join("missing/version"))).expect_err("dir");
    assert!(e.to_string().contains("missing/version"), "{e}");
}

#[test]
fn the_walk_reads_each_parent_so_a_long_compare_never_hides_the_newest_commits() {
    // GitHub lists a compare's commits oldest first and cuts the list short (30 a page), so with
    // many commits after the tag the tip and its parents are not in it.
    // Catches: a walk that follows parents through the compare's list, which would judge the tip
    // alone and leave a ready parent unreleased.
    let mut gh = history(
        &[
            pushed(COMMIT, &sha(2), None, false),
            pushed(&sha(2), &sha(1), Some("0.3.56"), true),
        ],
        &[],
        &["crates/tierstore/src/lib.rs"],
    );
    for (_, v) in &mut gh.api {
        if v.get("status").is_some() {
            v["commits"] = json!([{ "sha": sha(40), "commit": { "message": "oldest" } }]);
        }
    }
    let (printed, _) = plan_tip(&gh, "walk-long");
    assert_eq!(printed[0], format!("release v0.3.56 of {}", sha(2)));
}

#[test]
fn a_root_commit_ends_the_walk() {
    // A history with no parent above the tag (the tag is on another line GitHub still calls an
    // ancestor). Catches: a missing parent read as a commit to judge.
    let mut gh = history(
        &[pushed(COMMIT, &sha(1), None, false)],
        &[],
        &["Cargo.lock"],
    );
    let path = format!("repos/{REPO}/commits/{COMMIT}");
    for (p, v) in &mut gh.api {
        if *p == path {
            v["parents"] = json!([]);
        }
        if v.get("status").is_some() {
            v["merge_base_commit"]["sha"] = json!(sha(5));
        }
    }
    let (_, written) = plan_tip(&gh, "walk-root");
    assert!(written.starts_with("release=false\n"), "{written}");
    assert!(!written.contains("not judged"), "{written}");
}

#[test]
fn a_parent_github_does_not_know_or_a_compare_without_its_base_is_an_error() {
    // Catches: an unknown commit on main's history, or a compare that does not name the tag's
    // commit, taken for the end of the walk (a plan that silently judges less than it says).
    let mut gh = history(
        &[
            pushed(COMMIT, &sha(2), None, false),
            pushed(&sha(2), &sha(1), Some("0.3.56"), true),
        ],
        &[],
        &["Cargo.lock"],
    );
    gh.api
        .retain(|(p, _)| *p != format!("repos/{REPO}/commits/{COMMIT}"));
    let e = plan(&gh, &request(None, "", None)).expect_err("unknown");
    assert_eq!(e.to_string(), format!("GitHub does not know {COMMIT}"));
    let mut gh = history(
        &[pushed(COMMIT, &sha(1), None, false)],
        &[],
        &["Cargo.lock"],
    );
    for (_, v) in &mut gh.api {
        if let Some(map) = v.as_object_mut() {
            map.remove("merge_base_commit");
        }
    }
    let e = plan(&gh, &request(None, "", None)).expect_err("base");
    assert_eq!(
        e.to_string(),
        format!("GitHub's compare names no commit for {TAG}")
    );
}
