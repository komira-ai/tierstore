use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example/tierstore";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-plan-unit"), name)
}

/// GitHub made to answer: a path that is not listed answers 404. Every path asked is kept.
#[derive(Default)]
struct FakeGh {
    api: Vec<(String, Value)>,
    asked: RefCell<Vec<String>>,
}

impl Gh for FakeGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        self.asked.borrow_mut().push(path.to_owned());
        Ok(self
            .api
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| v.clone()))
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        panic!("a plan runs no gh command: {args:?}");
    }
}

fn runs_path(workflow: &str) -> String {
    format!(
        "repos/{REPO}/actions/workflows/{workflow}/runs?head_sha={COMMIT}&event=push&branch=main&status=success&per_page=20"
    )
}

fn green(id: u64) -> Value {
    json!({ "workflow_runs": [{
        "id": id, "head_sha": COMMIT, "head_branch": "main",
        "event": "push", "conclusion": "success",
    }]})
}

fn tags_path() -> String {
    format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100")
}

fn compare_path(tag: &str) -> String {
    format!("repos/{REPO}/compare/{tag}...{COMMIT}")
}

fn refs(tags: &[&str]) -> Value {
    Value::Array(
        tags.iter()
            .map(|t| json!({ "ref": format!("refs/tags/{t}") }))
            .collect(),
    )
}

/// A repository whose `COMMIT` has green runs of both workflows, whose release tags are
/// `tags`, and which holds `files` and the commit messages `messages` after the highest of them.
fn repo(tags: &[&str], status: &str, files: &[&str], messages: &[&str]) -> FakeGh {
    let mut api = vec![
        (runs_path("artifacts.yml"), green(11)),
        (runs_path("ci.yml"), green(22)),
        (tags_path(), refs(tags)),
    ];
    let compared = json!({
        "status": status,
        "files": files.iter().map(|f| json!({ "filename": f })).collect::<Vec<_>>(),
        "commits": messages
            .iter()
            .map(|m| json!({ "commit": { "message": m } }))
            .collect::<Vec<_>>(),
    });
    for tag in tags.iter().chain(&["v0.3.1"]) {
        let path = compare_path(tag);
        if !api.iter().any(|(p, _)| *p == path) {
            api.push((path, compared.clone()));
        }
    }
    FakeGh {
        api,
        ..FakeGh::default()
    }
}

fn request<'a>(version: Option<&'a str>, freeze: &'a str, output: Option<&'a Path>) -> Request<'a> {
    Request {
        repo: REPO,
        commit: COMMIT,
        version,
        freeze,
        output,
    }
}

fn plan_of(gh: &FakeGh, version: Option<&str>) -> Vec<String> {
    plan(gh, &request(version, "", None)).expect("plan")
}

fn files(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

#[test]
fn the_next_tag_is_the_patch_after_the_latest_release() {
    // Catches: a bump of the wrong part, and a latest taken as text (0.9.0 above 0.10.0).
    assert_eq!(next_tag(Some("v0.3.1"), None).expect("tag"), "v0.3.2");
    assert_eq!(next_tag(Some("v0.10.9"), None).expect("tag"), "v0.10.10");
    assert_eq!(next_tag(Some("v2.0.0"), None).expect("tag"), "v2.0.1");
}

#[test]
fn a_version_file_above_the_latest_release_names_the_tag() {
    // Catches: VERSION ignored, a leading `v` or a trailing newline refused, and the patch
    // bump applied on top of it.
    assert_eq!(
        next_tag(Some("v0.3.1"), Some("0.4.0")).expect("tag"),
        "v0.4.0"
    );
    assert_eq!(
        next_tag(Some("v0.3.1"), Some("v0.4.0\n")).expect("tag"),
        "v0.4.0"
    );
    assert_eq!(
        next_tag(Some("v0.3.1"), Some("0.3.2")).expect("tag"),
        "v0.3.2"
    );
    assert_eq!(
        next_tag(Some("v0.3.1"), Some("1.0.0")).expect("tag"),
        "v1.0.0"
    );
}

#[test]
fn a_version_file_at_or_below_the_latest_release_is_a_floor_and_the_patch_bump_stands() {
    // Catches: a stale VERSION re-cutting a tag that exists (refused by publish, so every
    // later push would be red) or refusing every release until someone edits the file.
    for version in ["0.3.1", "0.3.0", "0.2.9", "0.0.1"] {
        assert_eq!(
            next_tag(Some("v0.3.1"), Some(version)).expect("tag"),
            "v0.3.2",
            "{version}"
        );
    }
}

#[test]
fn the_first_release_is_the_version_file_or_v0_1_0() {
    // Catches: no tag at all being a failure, or VERSION being ignored there.
    assert_eq!(next_tag(None, None).expect("tag"), "v0.1.0");
    assert_eq!(next_tag(None, Some("0.0.7")).expect("tag"), "v0.0.7");
}

#[test]
fn a_version_file_that_is_not_a_version_is_refused() {
    // Catches: garbage in VERSION becoming a tag; leading zeros, a prerelease suffix, a
    // missing part, an empty file and a number too large for a u64.
    for version in [
        "",
        "\n",
        "0.4",
        "0.4.0-rc1",
        "01.2.3",
        "a.b.c",
        "0.4.0.1",
        "99999999999999999999.0.0",
    ] {
        let e = next_tag(Some("v0.3.1"), Some(version)).expect_err(version);
        assert!(e.to_string().contains("VERSION"), "{version}: {e}");
    }
    let e = next_tag(Some("latest"), None).expect_err("latest");
    assert!(e.to_string().contains("latest"), "{e}");
}

#[test]
fn the_version_file_is_read_when_it_is_there() {
    // Catches: a missing file taken for an error, and a present one not read.
    let dir = scratch("version-file");
    assert_eq!(read_version(&dir.join("VERSION")).expect("read"), None);
    std::fs::write(dir.join("VERSION"), "0.4.0\n").expect("write");
    assert_eq!(
        read_version(&dir.join("VERSION")).expect("read").as_deref(),
        Some("0.4.0\n")
    );
    // A directory where the file should be is an error, not an absent file.
    assert!(read_version(&dir).is_err());
}

#[test]
fn only_docs_paths_are_docs() {
    // Catches: a pattern that is too wide (a source file under a directory named like a docs
    // path, a deeper `docs/`), too narrow, or a file that merely ends like one.
    for path in [
        "docs/ci-and-release.md",
        "docs/anything/at/all.png",
        "README.md",
        "MIGRATION.md",
        "crates/tierstore/README.md",
        "crates/tierstore/docs/install.md",
        "crates/tierstore/docs/diagram.svg",
        "crates/xtask/docs/x.txt",
        "crates/tierstore/site/content/index.html",
        "crates/tierstore/site/package-lock.json",
    ] {
        assert!(is_docs_path(path), "{path}");
    }
    for path in [
        "crates/tierstore/src/main.rs",
        "crates/tierstore/src/docs/mod.rs",
        "src/docs/mod.rs",
        "Cargo.lock",
        "Cargo.toml",
        "docs",
        "crates/docs/x.rs",
        "crates/tierstore/docsx/a.md.rs",
        "crates/tierstore/sites/a.html",
        "packaging/tierstore.service",
        ".github/workflows/release.yml",
        "LICENSE",
        "notes.md.sh",
        "VERSION",
    ] {
        assert!(!is_docs_path(path), "{path}");
    }
}

#[test]
fn a_change_of_docs_alone_is_docs_only_and_one_other_file_is_not() {
    // Catches: `any` taken for `all`, and a mixed change skipped.
    assert!(docs_only(&files(&["README.md", "docs/ci-and-release.md"])));
    assert!(docs_only(&[]));
    assert!(!docs_only(&files(&[
        "README.md",
        "crates/tierstore/src/lib.rs"
    ])));
    assert!(!docs_only(&files(&["Cargo.lock"])));
}

#[test]
fn the_list_of_docs_paths_is_in_the_guide() {
    // Catches: the list changed in code and not in the guide that tells a maintainer which
    // pushes release.
    let guide = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ci-and-release.md"),
    )
    .expect("the guide");
    for pattern in DOCS_PATHS {
        assert!(
            guide.contains(&format!("`{pattern}`")),
            "docs/ci-and-release.md does not list `{pattern}`"
        );
    }
}

#[test]
fn a_push_that_changes_code_is_released_as_the_next_patch_with_its_titles() {
    // Catches: a code push skipped, the tag, the commit or the titles missing from the
    // outputs, and a long list of commits not cut short.
    let dir = scratch("plan-code");
    let out = dir.join("out");
    let messages: Vec<String> = (1..=22)
        .map(|n| format!("change {n} (#{n})\n\nA body that is not a title."))
        .collect();
    let messages: Vec<&str> = messages.iter().map(String::as_str).collect();
    let gh = repo(
        &["v0.3.0", "v0.3.1", "v0.2.0"],
        "ahead",
        &["README.md", "crates/tierstore/src/lib.rs"],
        &messages,
    );
    let printed = plan(&gh, &request(None, "", Some(&out))).expect("plan");
    assert!(printed.contains(&"release=true".to_owned()), "{printed:?}");
    assert!(printed.contains(&"tag=v0.3.2".to_owned()), "{printed:?}");
    let written = std::fs::read_to_string(&out).expect("out");
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines.len(), 4, "{written}");
    assert_eq!(lines[0], "release=true");
    assert_eq!(lines[1], "tag=v0.3.2");
    assert_eq!(lines[2], format!("commit={COMMIT}"));
    assert!(
        lines[3].starts_with("note=Changes since v0.3.1: change 1 (#1); change 2 (#2);"),
        "{written}"
    );
    assert!(lines[3].contains("change 20 (#20)"), "{written}");
    assert!(!lines[3].contains("change 21"), "{written}");
    assert!(lines[3].ends_with("; and 2 more."), "{written}");
    assert!(!lines[3].contains("A body"), "{written}");
}

#[test]
fn a_release_without_commit_messages_says_where_it_came_from() {
    // Catches: an empty note, and blank or control characters (a new line would end the
    // output line and inject another) kept in a title.
    let gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &[]);
    let printed = plan_of(&gh, None);
    assert!(
        printed.contains(&format!("note=The push run of {COMMIT} on main.")),
        "{printed:?}"
    );
    let gh = repo(
        &["v0.3.1"],
        "ahead",
        &["Cargo.lock"],
        &["\n\ntail", "  \t\n", "a\u{7}b\rc"],
    );
    let printed = plan_of(&gh, None);
    assert!(
        printed.contains(&"note=Changes since v0.3.1: a b c.".to_owned()),
        "{printed:?}"
    );
}

#[test]
fn a_version_file_names_the_tag_of_a_code_push() {
    // Catches: VERSION read by the plan but not used for the tag.
    let gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["bump"]);
    let printed = plan_of(&gh, Some("0.4.0\n"));
    assert!(printed.contains(&"tag=v0.4.0".to_owned()), "{printed:?}");
    let gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["bump"]);
    let e = plan(&gh, &request(Some("nonsense"), "", None)).expect_err("version");
    assert!(e.to_string().contains("VERSION"), "{e}");
}

#[test]
fn a_push_that_changes_docs_alone_makes_no_release() {
    // Catches: a docs push released, the reason missing, and a tag chosen for nothing.
    let dir = scratch("plan-docs");
    let out = dir.join("out");
    let gh = repo(
        &["v0.3.1"],
        "ahead",
        &[
            "README.md",
            "docs/ci-and-release.md",
            "crates/tierstore/docs/x.md",
        ],
        &["docs: reword"],
    );
    let printed = plan(&gh, &request(None, "", Some(&out))).expect("plan");
    assert!(printed[0].contains("only docs"), "{printed:?}");
    let written = std::fs::read_to_string(&out).expect("out");
    assert!(written.starts_with("release=false\nreason="), "{written}");
    assert!(!written.contains("tag="), "{written}");
}

#[test]
fn a_mixed_push_is_released() {
    // Catches: a change of docs and one source file being skipped as docs.
    let gh = repo(
        &["v0.3.1"],
        "ahead",
        &["README.md", "crates/tierstore/src/lib.rs", "docs/x.md"],
        &["fix"],
    );
    assert!(plan_of(&gh, None).contains(&"release=true".to_owned()));
}

#[test]
fn a_truncated_file_list_is_treated_as_code() {
    // Catches: GitHub's cut of a compare at 300 files hiding a source file behind 300 docs.
    let docs: Vec<String> = (0..300).map(|n| format!("docs/page{n}.md")).collect();
    let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
    let gh = repo(&["v0.3.1"], "ahead", &docs, &["many pages"]);
    assert!(plan_of(&gh, None).contains(&"release=true".to_owned()));
    let gh = repo(&["v0.3.1"], "ahead", &docs[..299], &["many pages"]);
    assert!(plan_of(&gh, None).contains(&"release=false".to_owned()));
}

#[test]
fn the_freeze_stops_the_plan_before_it_asks_anything() {
    // Catches: RELEASE_FREEZE set releasing, a freeze that still calls GitHub, a freeze that
    // fails open on a value other than exactly `true` (TRUE, yes, 1, on, any word), and an
    // unset, empty or `false` (any case) variable freezing.
    let gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["fix"]);
    for value in ["true", "TRUE", "True", "yes", "1", "on", "freeze", " true "] {
        let gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["fix"]);
        let printed = plan(&gh, &request(None, value, None)).expect("plan");
        assert!(printed.contains(&"release=false".to_owned()), "{value}");
        assert!(printed[0].contains("RELEASE_FREEZE"), "{printed:?}");
        assert!(
            gh.asked.borrow().is_empty(),
            "{value}: {:?}",
            gh.asked.borrow()
        );
    }
    for value in ["", "false", "FALSE", "False", "  ", " false "] {
        let printed = plan(&gh, &request(None, value, None)).expect("plan");
        assert!(printed.contains(&"release=true".to_owned()), "{value:?}");
    }
}

#[test]
fn a_rename_into_docs_is_a_change_of_the_file_it_was() {
    // Catches: a source file moved into docs/ reading as a docs-only push because only the new
    // name is looked at, and a move between docs paths being released.
    let renamed = |from: &str, to: &str| {
        let mut gh = repo(&["v0.3.1"], "ahead", &[], &["move"]);
        let path = compare_path("v0.3.1");
        for (p, v) in &mut gh.api {
            if *p == path {
                v["files"] = json!([{
                    "filename": to, "previous_filename": from, "status": "renamed"
                }]);
            }
        }
        gh
    };
    let gh = renamed("crates/tierstore/src/lib.rs", "docs/lib.md");
    assert!(plan_of(&gh, None).contains(&"release=true".to_owned()));
    let gh = renamed("docs/a.md", "docs/b.md");
    assert!(plan_of(&gh, None).contains(&"release=false".to_owned()));
    let gh = renamed("docs/a.md", "crates/tierstore/src/a.rs");
    assert!(plan_of(&gh, None).contains(&"release=true".to_owned()));
}

#[test]
fn a_commit_without_both_green_runs_waits_for_the_run_that_finishes_last() {
    // Catches: a release started by the first of the two workflows to finish, and a missing
    // run being an error (the run that finished first would be red for being early).
    for missing in ["artifacts.yml", "ci.yml"] {
        let mut gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["fix"]);
        let path = runs_path(missing);
        gh.api.retain(|(p, _)| *p != path);
        let printed = plan_of(&gh, None);
        assert!(printed.contains(&"release=false".to_owned()), "{missing}");
        assert!(printed[0].contains(missing), "{printed:?}");
        assert!(
            !gh.asked.borrow().contains(&tags_path()),
            "{missing}: tags read"
        );
    }
}

#[test]
fn a_commit_a_release_already_covers_makes_no_release() {
    // Catches: the second of two plans of one commit (both workflows finished together) cutting
    // another tag, and an old commit being released after a newer one.
    for status in ["identical", "behind"] {
        let gh = repo(&["v0.3.1"], status, &["Cargo.lock"], &["fix"]);
        let printed = plan_of(&gh, None);
        assert!(printed.contains(&"release=false".to_owned()), "{status}");
        assert!(printed[0].contains("already"), "{printed:?}");
    }
}

#[test]
fn a_latest_tag_off_the_history_of_the_commit_is_refused() {
    // Catches: a diverged tag measured as if it were an ancestor, and an unknown compare
    // taken for no change.
    let gh = repo(&["v0.3.1"], "diverged", &["Cargo.lock"], &["fix"]);
    let e = plan(&gh, &request(None, "", None)).expect_err("diverged");
    assert!(e.to_string().contains("not an ancestor"), "{e}");
    let mut gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["fix"]);
    gh.api.retain(|(p, _)| *p != compare_path("v0.3.1"));
    let e = plan(&gh, &request(None, "", None)).expect_err("unknown");
    assert!(e.to_string().contains("v0.3.1"), "{e}");
}

#[test]
fn a_repository_without_a_release_tag_releases_the_first_commit() {
    // Catches: no tag at all being refused, a non-release tag counting as the latest, and a
    // compare asked against a tag that does not exist.
    let mut gh = repo(&["latest", "v1.0.0-rc1", "v01.0.0"], "ahead", &[], &[]);
    gh.api.retain(|(p, _)| !p.contains("compare"));
    // Refs that are not tags, and one without a name, are not release tags either.
    for (path, listed) in &mut gh.api {
        if *path == tags_path() {
            listed
                .as_array_mut()
                .expect("refs")
                .extend([json!({ "ref": "refs/heads/v2" }), json!({})]);
        }
    }
    let printed = plan_of(&gh, None);
    assert!(printed.contains(&"tag=v0.1.0".to_owned()), "{printed:?}");
    assert!(
        printed.contains(&format!("note=The push run of {COMMIT} on main.")),
        "{printed:?}"
    );
    assert!(
        !gh.asked.borrow().iter().any(|p| p.contains("compare")),
        "{:?}",
        gh.asked.borrow()
    );
}

#[test]
fn a_hundred_tags_are_more_than_one_page_and_are_refused() {
    // Catches: the highest tag taken from a page that may not hold it, which would pick a tag
    // that exists.
    let names: Vec<String> = (0..100).map(|n| format!("v0.0.{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let gh = repo(&names, "ahead", &["Cargo.lock"], &["fix"]);
    let e = plan(&gh, &request(None, "", None)).expect_err("page");
    assert!(e.to_string().contains("page"), "{e}");
}

#[test]
fn a_plan_asks_for_a_full_commit() {
    // Catches: a short or upper-case commit reaching the API paths.
    let gh = repo(&["v0.3.1"], "ahead", &["Cargo.lock"], &["fix"]);
    let mut r = request(None, "", None);
    r.commit = "0123456789ab";
    assert!(matches!(plan(&gh, &r), Err(Error::Usage(_))));
}

#[test]
fn a_manual_release_passes_its_tag_commit_and_note_on() {
    // Catches: the manual run not producing the outputs the jobs after the plan read, a note
    // with a line break ending its output line early, and a bad tag or commit accepted.
    let dir = scratch("manual");
    let out = dir.join("out");
    let none = FakeGh::default();
    let printed =
        manual(&none, REPO, "v1.2.3", COMMIT, "first\nsecond", Some(&out)).expect("manual");
    assert!(printed.contains(&"tag=v1.2.3".to_owned()), "{printed:?}");
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        format!("release=true\ntag=v1.2.3\ncommit={COMMIT}\nnote=first second\n")
    );
    assert!(manual(&none, REPO, "v1.2.3", COMMIT, "", None).is_ok());
    assert!(matches!(
        manual(&none, REPO, "1.2", COMMIT, "", None),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        manual(&none, REPO, "v1.2.3", "abc", "", None),
        Err(Error::Usage(_))
    ));
}

#[test]
fn a_manual_release_of_a_commit_a_tag_already_covers_is_refused() {
    // Catches: a second tag name made for a commit that is already released (the commit is the
    // highest tag's, or older), while a commit after the tag, a tag line the commit is not on,
    // and a commit GitHub does not know (verify refuses it later) are not stopped here.
    for status in ["identical", "behind"] {
        let gh = repo(&["v0.3.1"], status, &["Cargo.lock"], &["fix"]);
        let e = manual(&gh, REPO, "v0.3.2", COMMIT, "", None).expect_err(status);
        assert!(
            e.to_string().contains("v0.3.1 already covers"),
            "{status}: {e}"
        );
    }
    for status in ["ahead", "diverged"] {
        let gh = repo(&["v0.3.1"], status, &["Cargo.lock"], &["fix"]);
        assert!(
            manual(&gh, REPO, "v0.3.2", COMMIT, "", None).is_ok(),
            "{status}"
        );
    }
    let mut gh = repo(&["v0.3.1"], "ahead", &[], &[]);
    gh.api.retain(|(p, _)| *p != compare_path("v0.3.1"));
    assert!(manual(&gh, REPO, "v0.3.2", COMMIT, "", None).is_ok());
}

#[test]
fn the_version_of_a_commit_is_the_one_the_plan_would_tag() {
    // Catches: the build and the release computing the version two ways, so that the binary
    // says one number and the tag another; the build's version reading anything but the tags
    // and the VERSION file (it runs before the tests and the signing, beside them).
    for (tags, file, want) in [
        (vec!["v0.3.0", "v0.3.1"], None, "0.3.2"),
        (vec!["v0.3.1"], Some("0.4.0\n"), "0.4.0"),
        (vec!["v0.3.1"], Some("0.2.0"), "0.3.2"),
        (vec!["v0.9.0", "v0.10.0"], None, "0.10.1"),
        (vec![], None, "0.1.0"),
        (vec![], Some("v0.0.7"), "0.0.7"),
    ] {
        let gh = repo(&tags, "ahead", &["Cargo.lock"], &["fix"]);
        let printed = version(&gh, REPO, file, None).expect("version");
        assert_eq!(
            printed,
            vec![format!("version={want}")],
            "{tags:?} {file:?}"
        );
        assert_eq!(*gh.asked.borrow(), vec![tags_path()], "{tags:?}");
        let planned = plan_of(&gh, file);
        assert!(
            planned.contains(&format!("tag=v{want}")),
            "{tags:?} {file:?}: {planned:?}"
        );
    }
}

#[test]
fn the_version_is_appended_to_the_output_file_and_a_bad_input_is_an_error() {
    // Catches: the answer not reaching `GITHUB_OUTPUT`, a VERSION that is not a version being
    // built under, and a page of tags that may hide the highest.
    let dir = scratch("version-out");
    let out = dir.join("out");
    let gh = repo(&["v0.3.1"], "ahead", &[], &[]);
    version(&gh, REPO, None, Some(&out)).expect("version");
    version(&gh, REPO, Some("0.5.0"), Some(&out)).expect("version");
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "version=0.3.2\nversion=0.5.0\n"
    );
    let e = version(&gh, REPO, Some("soon"), None).expect_err("bad");
    assert!(e.to_string().contains("VERSION"), "{e}");
    let names: Vec<String> = (0..100).map(|n| format!("v0.0.{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let gh = repo(&names, "ahead", &[], &[]);
    assert!(version(&gh, REPO, None, None).is_err());
}
