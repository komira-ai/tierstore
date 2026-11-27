//! The Status lines of the design pages (`docs/design/*.md`). A Status line says what of its
//! page the code on main builds (AGENTS.md P10); these checks hold the parts of that a text can
//! show. The checks are pure functions of the texts, so each is tested against edited texts as
//! well as against the real files.
//!
//! Catches:
//! - a design page with no Status line: a paragraph that starts with `Status:` at the start of a
//!   line, before the page's first section heading (`## `). A `Status:` inside a sentence, or
//!   only under a section, is not one;
//! - a Status line that still makes a claim main has made false, from the list of such claims
//!   in `STALE` (the Status line of `virtual-build-fs.md` saying "nothing here is built", among
//!   others); the same words below the Status line are not such a claim;
//! - a section of `fetch-cache-serve.md` the owner decided not to build (10, the storage tier, and
//!   24, its operations and repair) whose first paragraph is not a Status line saying
//!   "not planned".

use std::path::PathBuf;
use std::process::Command;

/// The page that lists the status of the others (when present); it has no Status line of its own.
const STATUS_PAGE: &str = "docs/design/status.md";

/// Claims a page's Status line made that main has made false: the page, and words its Status
/// line no longer says.
const STALE: &[(&str, &str)] = &[
    ("docs/design/virtual-build-fs.md", "nothing here is built"),
    ("docs/design/virtual-build-fs.md", "phase 1 starts now"),
    ("docs/design/fetch-cache-serve.md", "nothing in it was run"),
    (
        "docs/design/one-service.md",
        "No code changes come with this document",
    ),
    (
        "docs/design/pluggable-interfaces.md",
        "No code changes come with this document",
    ),
    ("docs/design/distribution.md", "proposal, for review"),
    ("docs/design/overwrite.md", "designed here, not built"),
    (
        "docs/design/put-without-lock.md",
        "proposed (CEO direction, 2026-11-05), not built",
    ),
    ("docs/design/prefetch-api.md", "not built.** This text"),
    (
        "docs/design/node-local-trees.md",
        "not built.** This is a design",
    ),
    ("docs/design/tierstore-design.md", "probe_tiers"),
];

/// The sections of `fetch-cache-serve.md` the owner decided not to build.
const NOT_PLANNED: &[&str] = &["10", "24"];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A file of the repository, by its path from the root.
fn read(rel: &str) -> String {
    let path = root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The tracked design pages, by `git ls-files`.
fn design_pages() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z", "--", "docs/design/*.md"])
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| panic!("run git ls-files: {e}"));
    assert!(out.status.success(), "git ls-files failed: {}", out.status);
    String::from_utf8(out.stdout)
        .unwrap_or_else(|e| panic!("git ls-files wrote a path that is not UTF-8: {e}"))
        .split('\0')
        .filter(|p| !p.is_empty() && !p.contains("/diagrams/"))
        .map(str::to_owned)
        .collect()
}

/// The paragraph that starts at `lines[start]`: that line and the next ones up to a blank line
/// or a heading, joined with spaces.
fn paragraph(lines: &[&str], start: usize) -> String {
    lines[start..]
        .iter()
        .take_while(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Status line of a page: the paragraph that starts with `Status:` at the start of a line,
/// before the page's first section heading.
fn status_line(page: &str) -> Option<String> {
    let lines: Vec<&str> = page.lines().collect();
    let head = lines
        .iter()
        .position(|l| l.starts_with("## "))
        .unwrap_or(lines.len());
    (0..head)
        .find(|&i| lines[i].starts_with("Status:"))
        .map(|i| paragraph(&lines, i))
}

/// The first paragraph under the heading of section `number` (`## 10. ...` or `### 10 ...`), if
/// it is a Status line; `Err` names what is wrong.
fn section_status(page: &str, number: &str) -> Result<String, String> {
    let lines: Vec<&str> = page.lines().collect();
    let heading = lines
        .iter()
        .position(|l| {
            l.strip_prefix("## ")
                .and_then(|rest| rest.strip_prefix(number))
                .is_some_and(|rest| rest.starts_with(". "))
        })
        .ok_or_else(|| format!("no heading of section {number}"))?;
    let first = (heading + 1..lines.len())
        .find(|&i| !lines[i].trim().is_empty())
        .ok_or_else(|| format!("section {number} has no text"))?;
    if lines[first].starts_with("Status:") {
        Ok(paragraph(&lines, first))
    } else {
        Err(format!(
            "section {number} does not open with a Status line: line {} is {:?}",
            first + 1,
            lines[first]
        ))
    }
}

#[test]
fn every_design_page_has_a_status_line_before_its_first_section() {
    let pages = design_pages();
    assert!(
        pages
            .iter()
            .any(|p| p == "docs/design/fetch-cache-serve.md"),
        "git ls-files found no design pages: {pages:?}"
    );
    let missing: Vec<&String> = pages
        .iter()
        .filter(|p| *p != STATUS_PAGE && status_line(&read(p)).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "no Status line before the first section: {missing:?}"
    );
}

#[test]
fn the_virtual_build_fs_status_line_does_not_say_nothing_is_built() {
    let line = status_line(&read("docs/design/virtual-build-fs.md"))
        .unwrap_or_else(|| panic!("virtual-build-fs.md has no Status line"));
    assert!(
        !line.contains("nothing here is built"),
        "the Status line still says nothing is built: {line}"
    );
}

/// The claims of `stale` that the Status line of their page still makes, each as
/// `page: "words"`; `text_of` gives a page's text.
fn kept_claims(stale: &[(&str, &str)], text_of: impl Fn(&str) -> String) -> Vec<String> {
    stale
        .iter()
        .filter_map(|(page, words)| {
            let line = status_line(&text_of(page))?;
            line.contains(words).then(|| format!("{page}: {words:?}"))
        })
        .collect()
}

#[test]
fn no_status_line_keeps_a_claim_main_has_made_false() {
    let kept = kept_claims(STALE, read);
    assert!(kept.is_empty(), "stale claims kept: {kept:?}");
}

/// Asserts that a claim kept in a Status line is named, and that the same words elsewhere in
/// the page, or a Status line without them, are not.
#[test]
fn a_stale_claim_is_named_only_in_its_page_s_status_line() {
    let pages = |page: &str| match page {
        "kept.md" => "# K\n\nStatus: proposed; nothing here is built.\n\n## 1. A\n".to_owned(),
        "below.md" => "# B\n\nStatus: built.\n\n## 1. A\n\nnothing here is built\n".to_owned(),
        _ => "# N\n\nNo status.\n".to_owned(),
    };
    let stale = [
        ("kept.md", "nothing here is built"),
        ("below.md", "nothing here is built"),
        ("none.md", "nothing here is built"),
    ];
    assert_eq!(
        kept_claims(&stale, pages),
        vec!["kept.md: \"nothing here is built\"".to_owned()]
    );
}

#[test]
fn the_storage_tier_sections_of_fetch_cache_serve_say_not_planned() {
    let page = read("docs/design/fetch-cache-serve.md");
    for number in NOT_PLANNED {
        let line = section_status(&page, number).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            line.contains("not planned"),
            "section {number}'s Status line does not say not planned: {line}"
        );
    }
}

/// Asserts that a Status line is the paragraph at the start of a line before the first section,
/// and nothing else is.
#[test]
fn a_status_line_is_found_only_at_the_head_of_a_page() {
    assert_eq!(
        status_line("# T\n\nStatus: **built**, all\nof it.\n\nMore.\n\n## 1. A\n").as_deref(),
        Some("Status: **built**, all of it.")
    );
    assert_eq!(
        status_line("# T\n\nIntro.\n\nStatus: built\n## 1. A\n").as_deref(),
        Some("Status: built")
    );
    assert_eq!(
        status_line("# T\n\nIntro.\n\n## 1. A\n\nStatus: built\n"),
        None
    );
    assert_eq!(status_line("# T\n\nThe Status: built\n"), None);
    assert_eq!(status_line("# T\n\n **Status:** built\n"), None);
    assert_eq!(status_line(""), None);
}

/// Asserts that a section's Status line is its first paragraph, and that a section with
/// another first paragraph, no text or no heading is named.
#[test]
fn a_section_status_line_is_the_first_paragraph_under_its_heading() {
    let page = "# T\n\n## 1. One\n\nStatus: **not planned**.\n\n## 10. Ten\n\nText.\n\nStatus: x\n\n## 2. Two\n";
    assert_eq!(
        section_status(page, "1").as_deref(),
        Ok("Status: **not planned**.")
    );
    assert_eq!(
        section_status(page, "10"),
        Err("section 10 does not open with a Status line: line 9 is \"Text.\"".to_owned())
    );
    assert_eq!(
        section_status(page, "3"),
        Err("no heading of section 3".to_owned())
    );
    assert_eq!(
        section_status("## 4. Four\n\n", "4"),
        Err("section 4 has no text".to_owned())
    );
    assert_eq!(
        section_status("### 1.5 Sub\n\nStatus: x\n", "1"),
        Err("no heading of section 1".to_owned())
    );
}
