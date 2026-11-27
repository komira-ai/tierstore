//! The Status lines of the design pages (`docs/design/*.md`) and the status page that lists what
//! each one plans and what main builds (`docs/design/status.md`). A Status line says what of its
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
//!   "not planned";
//! - a status page whose summary does not count its tables: for each page, the number of rows of
//!   each status in the page's tables equals the summary's cell, and every row's status is one of
//!   the statuses the page defines. A `\|` inside a cell is text, not a cell border.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// The page that lists the status of the others; it has no Status line of its own.
const STATUS_PAGE: &str = "docs/design/status.md";

/// The statuses a row of the status page may have, in the order of the summary's columns.
const STATUSES: &[&str] = &[
    "built",
    "partial",
    "unbuilt",
    "superseded",
    "out-of-repo",
    "not-code",
    "not-planned",
];

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

/// The cells of a table row, trimmed: the text between the `|` that are not escaped as `\|`.
fn cells(row: &str) -> Vec<String> {
    let inner = row.trim().trim_start_matches('|');
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('\\');
                cell.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cell).trim().to_owned()),
            _ => cell.push(c),
        }
    }
    if !cell.trim().is_empty() {
        out.push(cell.trim().to_owned());
    }
    out
}

/// Counts of each status, by page.
type Counts = BTreeMap<String, BTreeMap<String, usize>>;

/// What is wrong with the status page `text`: a row with a status not in `STATUSES`, and each
/// cell of the summary that differs from the count of the page's rows.
fn status_page_findings(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut summary = Counts::new();
    let mut rows = Counts::new();
    let mut in_summary = false;
    let mut page: Option<String> = None;
    let mut status_column: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        if let Some(heading) = line.strip_prefix("## ") {
            in_summary = heading == "Summary";
            page = heading
                .strip_prefix('[')
                .and_then(|rest| rest.split_once(']'))
                .map(|(name, _)| name.to_owned());
            status_column = None;
            continue;
        }
        if !line.starts_with('|') {
            status_column = None;
            continue;
        }
        let row = cells(line);
        if row
            .iter()
            .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
        {
            continue;
        }
        if in_summary {
            let Some(name) = row
                .first()
                .and_then(|c| c.strip_prefix('['))
                .and_then(|rest| rest.split_once(']'))
                .map(|(name, _)| name.to_owned())
            else {
                continue;
            };
            let counts = summary.entry(name.clone()).or_default();
            for (status, cell) in STATUSES.iter().zip(row.iter().skip(1)) {
                match cell.parse::<usize>() {
                    Ok(n) => {
                        counts.insert((*status).to_owned(), n);
                    }
                    Err(_) => found.push(format!(
                        "line {}: the summary's {status} cell of {name} is {cell:?}, not a count",
                        i + 1
                    )),
                }
            }
            if row.len() != STATUSES.len() + 2 {
                found.push(format!(
                    "line {}: the summary row of {name} has {} cells, not {}",
                    i + 1,
                    row.len(),
                    STATUSES.len() + 2
                ));
            }
            continue;
        }
        let Some(name) = page.as_ref() else {
            continue;
        };
        match status_column {
            None => status_column = row.iter().position(|c| c == "Status"),
            Some(column) => {
                let status = row.get(column).map_or("", String::as_str);
                if STATUSES.contains(&status) {
                    *rows
                        .entry(name.clone())
                        .or_default()
                        .entry(status.to_owned())
                        .or_default() += 1;
                } else {
                    found.push(format!(
                        "line {}: status {status:?} of a row of {name} is not one of {}",
                        i + 1,
                        STATUSES.join(", ")
                    ));
                }
            }
        }
    }
    for name in summary.keys().chain(rows.keys()) {
        for status in STATUSES {
            let said = summary
                .get(name)
                .and_then(|c| c.get(*status))
                .copied()
                .unwrap_or(0);
            let counted = rows
                .get(name)
                .and_then(|c| c.get(*status))
                .copied()
                .unwrap_or(0);
            if said != counted {
                let finding =
                    format!("{name}: the summary says {said} {status}, the tables have {counted}");
                if !found.contains(&finding) {
                    found.push(finding);
                }
            }
        }
    }
    found
}

#[test]
fn every_design_page_has_a_status_line_before_its_first_section() {
    let pages = design_pages();
    assert!(
        pages.iter().any(|p| p == STATUS_PAGE),
        "git ls-files found no {STATUS_PAGE} among {pages:?}"
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

#[test]
fn the_status_page_summary_counts_its_rows() {
    let found = status_page_findings(&read(STATUS_PAGE));
    assert!(found.is_empty(), "{}", found.join("\n"));
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

/// Asserts that an escaped `\|` stays inside its cell.
#[test]
fn a_table_row_splits_at_unescaped_bars_only() {
    assert_eq!(
        cells("| a | `x\\|y` | c |"),
        vec!["a".to_owned(), "`x\\|y`".to_owned(), "c".to_owned()]
    );
    assert_eq!(cells("|a|b"), vec!["a".to_owned(), "b".to_owned()]);
}

/// A status page with one design page `a.md`, its summary row `summary` and its table rows
/// `rows` (each the status of one row).
fn status_page(summary: &str, rows: &[&str]) -> String {
    let mut text = String::from(
        "# S\n\n## Summary\n\n| Page | built | partial | unbuilt | superseded | out-of-repo | not-code | not-planned | slice issues |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    text.push_str(summary);
    text.push_str("\n\n## [a.md](a.md)\n\n| Section | Item | Status | Built by | Unbuilt |\n|---|---|---|---|---|\n");
    for status in rows {
        text.push_str(&format!("| 1 | `a\\|b` item | {status} | x |  |\n"));
    }
    text
}

/// Asserts that a summary matching the rows passes, and that a wrong count, an unknown status,
/// a cell that is not a number and a short summary row are each named.
#[test]
fn the_status_page_check_names_a_wrong_count_or_an_unknown_status() {
    assert_eq!(
        status_page_findings(&status_page(
            "| [a.md](a.md) | 1 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |",
            &["built", "not-planned", "not-planned"]
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        status_page_findings(&status_page(
            "| [a.md](a.md) | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |",
            &["built", "unbuilt"]
        )),
        vec![
            "a.md: the summary says 2 built, the tables have 1".to_owned(),
            "a.md: the summary says 0 unbuilt, the tables have 1".to_owned(),
        ]
    );
    assert_eq!(
        status_page_findings(&status_page(
            "| [a.md](a.md) | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |",
            &["built", "done"]
        )),
        vec![
            "line 14: status \"done\" of a row of a.md is not one of built, partial, unbuilt, superseded, out-of-repo, not-code, not-planned"
                .to_owned()
        ]
    );
    assert_eq!(
        status_page_findings(&status_page(
            "| [a.md](a.md) | one | 0 | 0 | 0 | 0 | 0 |",
            &["built"]
        )),
        vec![
            "line 7: the summary's built cell of a.md is \"one\", not a count".to_owned(),
            "line 7: the summary row of a.md has 7 cells, not 9".to_owned(),
            "a.md: the summary says 0 built, the tables have 1".to_owned(),
        ]
    );
}
