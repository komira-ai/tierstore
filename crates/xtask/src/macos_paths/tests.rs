use super::*;
use crate::testdir::TestDir;

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-macos-paths-unit"), name)
}

#[test]
fn documentation_lint_configuration_templates_and_the_workflows_off_macos_are_skippable() {
    // Catches: a path of the lists, or a top-level page, judged as one the macOS job needs.
    for path in SKIPPABLE_FILES {
        assert!(skippable(path), "{path}");
    }
    for path in [
        "docs/ci-and-release.md",
        "docs/design/put-without-lock.md",
        "docs/anything/at/all.png",
        ".github/ISSUE_TEMPLATE/bug.yml",
        "README.md",
        "AGENTS.md",
        "CLAUDE.md",
        "SECURITY.md",
    ] {
        assert!(skippable(path), "{path}");
    }
}

#[test]
fn code_build_inputs_the_macos_workflows_and_anything_unlisted_are_not_skippable() {
    // Catches: a list that lets through the code, a build input, the two workflows that define
    // the macOS build, documentation a crate's tests or doctests read (under `crates/`), the
    // site and its install script, a workflow added later, and the edges of each pattern (a
    // directory's name alone, a longer name that starts like it, another case, another suffix).
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "VERSION",
        ".cargo/config.toml",
        ".config/nextest.toml",
        ".gitignore",
        "crates/tierstore/src/lib.rs",
        "crates/tierstore/docs/metrics.md",
        "crates/xtask/README.md",
        "packaging/README.md",
        "packaging/macos/tierstore.plist",
        "infra/site/install.sh",
        ".github/workflows/macos.yml",
        ".github/workflows/artifacts.yml",
        ".github/workflows/new.yml",
        "docs",
        "docs/",
        "docsx/a.md",
        ".github/ISSUE_TEMPLATE",
        "README.MD",
        "README.md.orig",
        ".md",
        "LICENSE.txt",
        "deny.toml/x",
        "",
    ] {
        assert!(!skippable(path), "{path}");
    }
}

#[test]
fn a_list_of_skippable_paths_alone_skips_the_job_and_says_how_many() {
    // Catches: a skip that does not reach the answer, and an empty entry (the NUL that ends the
    // list) counted as a path.
    assert_eq!(
        decide(b"README.md\0docs/x.md\0.github/workflows/ci.yml\0"),
        (
            false,
            "the macOS job is not needed: each of the 3 changed paths is one it builds nothing from"
                .to_owned()
        )
    );
    assert!(!decide(b"README.md").0);
}

#[test]
fn one_path_the_job_needs_among_skippable_ones_runs_it_and_names_that_path() {
    // Catches: a list judged by its first or its last path alone, and a rename out of a crate
    // (git names the old and the new path) judged by its new path.
    for listed in [
        &b"docs/x.md\0crates/tierstore/src/lib.rs\0README.md\0"[..],
        b"crates/tierstore/src/lib.rs\0docs/x.md\0",
        b"docs/x.md\0README.md\0crates/tierstore/src/lib.rs\0",
    ] {
        assert_eq!(
            decide(listed),
            (
                true,
                "the macOS job runs: crates/tierstore/src/lib.rs is not a path it may skip"
                    .to_owned()
            ),
            "{listed:?}"
        );
    }
}

#[test]
fn a_list_that_names_no_path_or_a_path_that_is_not_utf8_runs_the_job() {
    // Catches: an empty diff (one that could not be taken, or a merge that changes nothing)
    // read as "nothing the job needs", and a path that cannot be read as text skipped.
    for listed in [&b""[..], b"\0", b"\0\0"] {
        assert_eq!(
            decide(listed),
            (
                true,
                "the macOS job runs: the list of changed paths names none".to_owned()
            ),
            "{listed:?}"
        );
    }
    assert_eq!(
        decide(b"README.md\0docs/\xff.md\0"),
        (
            true,
            "the macOS job runs: the changed path \"docs/\u{fffd}.md\" is not UTF-8".to_owned()
        )
    );
}

#[test]
fn macos_paths_appends_its_answer_to_the_output_and_prints_why() {
    // Catches: the answer not reaching the output file, a file of earlier outputs overwritten
    // rather than appended to, and no output file being an error.
    let d = scratch("answer");
    let changed = d.join("changed");
    let output = d.join("github-output");
    std::fs::write(&changed, b"docs/x.md\0").expect("write");
    std::fs::write(&output, "earlier=1\n").expect("write");
    assert_eq!(
        macos_paths(&changed, Some(&output)).expect("judged"),
        [
            "the macOS job is not needed: each of the 1 changed paths is one it builds nothing from",
            "macos=false",
        ]
    );
    std::fs::write(&changed, b"Cargo.lock\0").expect("write");
    assert_eq!(
        macos_paths(&changed, Some(&output)).expect("judged"),
        [
            "the macOS job runs: Cargo.lock is not a path it may skip",
            "macos=true"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&output).expect("read"),
        "earlier=1\nmacos=false\nmacos=true\n"
    );
    assert_eq!(
        macos_paths(&changed, None).expect("judged"),
        [
            "the macOS job runs: Cargo.lock is not a path it may skip",
            "macos=true"
        ]
    );
}

#[test]
fn a_list_that_cannot_be_read_runs_the_job_and_an_output_that_cannot_be_written_fails() {
    // Catches: a missing list (the diff step wrote nothing) read as "nothing changed", and an
    // answer that could not be written passing, which would leave the output unset.
    let d = scratch("unreadable");
    let missing = d.join("missing");
    let output = d.join("github-output");
    let printed = macos_paths(&missing, Some(&output)).expect("judged");
    assert_eq!(printed.len(), 2);
    assert!(
        printed[0].starts_with(&format!(
            "the macOS job runs: the list {} cannot be read: ",
            missing.display()
        )),
        "{printed:?}"
    );
    assert_eq!(printed[1], "macos=true");
    assert_eq!(
        std::fs::read_to_string(&output).expect("read"),
        "macos=true\n"
    );
    let changed = d.join("changed");
    std::fs::write(&changed, b"docs/x.md\0").expect("write");
    let err = macos_paths(&changed, Some(&d)).expect_err("a directory is not an output file");
    assert!(
        err.to_string()
            .starts_with(&format!("append to {}: ", d.display())),
        "{err}"
    );
}
