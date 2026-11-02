//! `AGENTS.md` (the rules every change is written and reviewed against) and `CLAUDE.md`
//! (which imports it). The checks are pure functions of the texts, so each is tested
//! against edited texts as well as against the real files.
//!
//! Catches:
//! - a shared core that drifted: the text between the two marker lines of `AGENTS.md` is,
//!   byte for byte, `docs/agents/shared_core.md` (the first line that differs is named, in
//!   both files);
//! - a copy edited along with it: `docs/agents/shared_core.sha256` is
//!   `<sha256 of the copy, lowercase hex>  shared_core.md` and one newline, the form
//!   `sha256sum` writes, and the digest is the copy's;
//! - markers that cannot delimit the core: each of the two is a line of its own, exactly
//!   once, the begin before the end;
//! - a `CLAUDE.md` that no longer imports `AGENTS.md`: a line that is exactly `@AGENTS.md`,
//!   outside a fenced code block of backticks or tildes (an import inside a fence, inside a
//!   sentence or with anything after it, a carriage return included, is not read as one).

use std::path::PathBuf;

use sha2::{Digest, Sha256};

const BEGIN: &str = "<!-- shared-core:begin -->";
const END: &str = "<!-- shared-core:end -->";
const IMPORT: &str = "@AGENTS.md";
const COPY: &str = "docs/agents/shared_core.md";
const HASH: &str = "docs/agents/shared_core.sha256";

/// One line of a text split at `\n` only: its 1-based number, its text without the `\n`,
/// the byte offset where it starts, and the offset just after its `\n`.
#[derive(Clone, Copy)]
struct Line<'a> {
    number: usize,
    text: &'a str,
    start: usize,
    after: usize,
}

fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let stop = text[start..].find('\n').map_or(text.len(), |i| start + i);
        out.push(Line {
            number: out.len() + 1,
            text: &text[start..stop],
            start,
            after: (stop + 1).min(text.len()),
        });
        start = stop + 1;
    }
    out
}

/// The shared core of `agents` (the text after the begin line, up to the end line) and the
/// number of the line it starts on.
fn core(agents: &str) -> Result<(&str, usize), Vec<String>> {
    let all = lines(agents);
    let find = |marker: &str| -> Vec<Line<'_>> {
        all.iter().filter(|l| l.text == marker).copied().collect()
    };
    let (begins, ends) = (find(BEGIN), find(END));
    let errors: Vec<String> = [(BEGIN, &begins), (END, &ends)]
        .into_iter()
        .filter(|(_, found)| found.len() != 1)
        .map(|(marker, found)| {
            format!(
                "AGENTS.md: the line `{marker}` is there {} times, not once",
                found.len()
            )
        })
        .collect();
    if !errors.is_empty() {
        return Err(errors);
    }
    let (begin, end) = (begins[0], ends[0]);
    if end.start < begin.after {
        return Err(vec![format!(
            "AGENTS.md:{}: the end of the shared core comes before its begin (line {})",
            end.number, begin.number
        )]);
    }
    Ok((&agents[begin.after..end.start], begin.number + 1))
}

/// Nothing when `got` (the core of AGENTS.md, which starts on its line `first`) is `want`
/// (the copy); otherwise the first line where they differ, named in both files.
fn same_core(got: &str, first: usize, want: &str) -> Result<(), String> {
    if got == want {
        return Ok(());
    }
    let (g, w) = (lines(got), lines(want));
    let i = g
        .iter()
        .zip(&w)
        .take_while(|(a, b)| a.text == b.text)
        .count();
    if i == g.len() && i == w.len() {
        // Every line is the same: the two differ only in the newline at the very end.
        return Err(format!(
            "AGENTS.md:{}: the shared core and {COPY} differ in the newline that ends them",
            first + i
        ));
    }
    let show = |l: Option<&Line<'_>>| {
        l.map_or("the end of the text".to_owned(), |l| {
            format!("`{}`", l.text)
        })
    };
    Err(format!(
        "AGENTS.md:{}: the shared core differs from {COPY}:{}: AGENTS.md has {}, the copy has {}",
        first + i,
        i + 1,
        show(g.get(i)),
        show(w.get(i)),
    ))
}

/// Nothing when `hash_file` is exactly `<sha256 of copy>  shared_core.md\n`.
fn hash_matches(copy: &[u8], hash_file: &str) -> Result<(), String> {
    let want = format!("{}  shared_core.md\n", hex::encode(Sha256::digest(copy)));
    if hash_file == want {
        Ok(())
    } else {
        Err(format!(
            "{HASH} is {hash_file:?}, not {want:?} (run `sha256sum shared_core.md` in docs/agents)"
        ))
    }
}

/// Whether `claude` has a line that is exactly `@AGENTS.md` outside a fenced code block.
fn imports_agents(claude: &str) -> bool {
    // The open fence: its character and length; a closing fence is at least as long.
    let mut fence: Option<(char, usize)> = None;
    for line in lines(claude) {
        let line = line.text;
        let t = line.trim_start_matches(' ');
        let indent = line.len() - t.len();
        let ch = t.chars().next();
        let run = ch.map_or(0, |c| t.chars().take_while(|&x| x == c).count());
        // A backtick fence's info string holds no backtick (such a line is inline code).
        let is_fence = indent <= 3
            && matches!(ch, Some('`' | '~'))
            && run >= 3
            && !(ch == Some('`') && t[run..].contains('`'));
        match (fence, is_fence) {
            (None, true) => fence = ch.map(|c| (c, run)),
            (Some((c, n)), true) if ch == Some(c) && run >= n && t[run..].trim().is_empty() => {
                fence = None
            }
            (None, false) if line == IMPORT => return true,
            _ => {}
        }
    }
    false
}

/// A file of the repository, by its path from the root.
fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Catches the real AGENTS.md drifting from its copy, the copy from its hash, and CLAUDE.md
/// no longer importing AGENTS.md.
#[test]
fn agents_md_core_is_the_hashed_copy_and_claude_md_imports_it() {
    let agents = read("AGENTS.md");
    let copy = read(COPY);
    let (got, first) = core(&agents).unwrap_or_else(|e| panic!("{}", e.join("\n")));
    assert_eq!(same_core(got, first, &copy), Ok(()));
    assert_eq!(hash_matches(copy.as_bytes(), &read(HASH)), Ok(()));
    assert!(
        imports_agents(&read("CLAUDE.md")),
        "CLAUDE.md has no line `{IMPORT}`"
    );
}

/// Catches a copy emptied or cut short together with the core and its hash: the core holds
/// the five headings of the shared core and a rule of each.
#[test]
fn the_real_core_has_every_section_of_the_shared_core() {
    let copy = read(COPY);
    for heading in [
        "### Tests and coverage",
        "### Design and change shape",
        "### CI and workflows",
        "### PR text and commits",
        "### Secrets and configuration",
    ] {
        assert!(
            copy.lines().any(|l| l == heading),
            "{COPY} has no heading `{heading}`"
        );
    }
    for id in ["T1", "B8", "B9", "P4", "S1"] {
        let rule = format!("- {id} ");
        assert!(
            copy.lines().any(|l| l.starts_with(&rule)),
            "{COPY} has no rule {id}"
        );
    }
}

fn agents_with(core_text: &str) -> String {
    format!("# AGENTS.md\n\nintro\n\n{BEGIN}\n{core_text}{END}\n\n## 2. Rust\n")
}

const CORE: &str = "## 1. Shared core\n\n- T1 BLOCK. one\n- T2 SHOULD. two\n";

/// Catches a core read from the wrong place: the text between the markers, and the line
/// after the begin marker as its first line.
#[test]
fn core_is_the_text_between_the_marker_lines() {
    assert_eq!(core(&agents_with(CORE)), Ok((CORE, 6)));
    // An empty core: the markers on adjacent lines.
    assert_eq!(core(&agents_with("")), Ok(("", 6)));
    // The end marker as the file's last line, with no newline after it.
    assert_eq!(core(&format!("{BEGIN}\nx\n{END}")), Ok(("x\n", 2)));
}

/// Catches a marker that is missing, repeated, out of order, or not a line of its own.
#[test]
fn markers_that_cannot_delimit_the_core_are_refused() {
    let twice = |m: &str| format!("AGENTS.md: the line `{m}` is there 2 times, not once");
    let none = |m: &str| format!("AGENTS.md: the line `{m}` is there 0 times, not once");
    assert_eq!(core(""), Err(vec![none(BEGIN), none(END)]));
    assert_eq!(core("no markers\n"), Err(vec![none(BEGIN), none(END)]));
    assert_eq!(core(&format!("{BEGIN}\nx\n")), Err(vec![none(END)]));
    assert_eq!(core(&format!("x\n{END}\n")), Err(vec![none(BEGIN)]));
    assert_eq!(
        core(&format!("{BEGIN}\n{BEGIN}\nx\n{END}\n")),
        Err(vec![twice(BEGIN)])
    );
    assert_eq!(
        core(&format!("{BEGIN}\nx\n{END}\n{END}\n")),
        Err(vec![twice(END)])
    );
    assert_eq!(
        core(&format!("{END}\nx\n{BEGIN}\n")),
        Err(vec![
            "AGENTS.md:1: the end of the shared core comes before its begin (line 3)".to_owned()
        ])
    );
    // A marker with text around it, or a carriage return, is not a marker line.
    for bad in [
        format!(" {BEGIN}"),
        format!("{BEGIN} "),
        format!("{BEGIN}\r"),
        format!("see {BEGIN}"),
    ] {
        assert_eq!(
            core(&format!("{bad}\nx\n{END}\n")),
            Err(vec![none(BEGIN)]),
            "{bad:?}"
        );
    }
}

/// Catches an edited core: a changed, added or removed line is named by its line in
/// AGENTS.md and in the copy, and a difference only in the final newline is refused too.
#[test]
fn a_core_that_differs_from_the_copy_names_the_first_differing_line() {
    assert_eq!(same_core(CORE, 6, CORE), Ok(()));
    let changed = CORE.replace("two", "2");
    assert_eq!(
        same_core(&changed, 6, CORE),
        Err(format!(
            "AGENTS.md:9: the shared core differs from {COPY}:4: AGENTS.md has `- T2 SHOULD. 2`, the copy has `- T2 SHOULD. two`"
        ))
    );
    let added = format!("{CORE}- T3 BLOCK. three\n");
    assert_eq!(
        same_core(&added, 6, CORE),
        Err(format!(
            "AGENTS.md:10: the shared core differs from {COPY}:5: AGENTS.md has `- T3 BLOCK. three`, the copy has the end of the text"
        ))
    );
    assert_eq!(
        same_core(CORE, 6, &added),
        Err(format!(
            "AGENTS.md:10: the shared core differs from {COPY}:5: AGENTS.md has the end of the text, the copy has `- T3 BLOCK. three`"
        ))
    );
    let crlf = CORE.replacen('\n', "\r\n", 1);
    assert_eq!(
        same_core(&crlf, 6, CORE),
        Err(format!(
            "AGENTS.md:6: the shared core differs from {COPY}:1: AGENTS.md has `## 1. Shared core\r`, the copy has `## 1. Shared core`"
        ))
    );
    assert_eq!(
        same_core(CORE.trim_end_matches('\n'), 6, CORE),
        Err(format!(
            "AGENTS.md:10: the shared core and {COPY} differ in the newline that ends them"
        ))
    );
    // A core emptied in AGENTS.md alone.
    assert_eq!(
        same_core("", 6, CORE),
        Err(format!(
            "AGENTS.md:6: the shared core differs from {COPY}:1: AGENTS.md has the end of the text, the copy has `## 1. Shared core`"
        ))
    );
}

/// Catches a copy edited without its hash, and a hash file in any form but the one
/// `sha256sum` writes.
#[test]
fn the_hash_file_is_the_copys_sha256sum_line() {
    let copy = CORE.as_bytes();
    let hex = hex::encode(Sha256::digest(copy));
    let good = format!("{hex}  shared_core.md\n");
    assert_eq!(hash_matches(copy, &good), Ok(()));
    for bad in [
        // The copy edited: the hash of the text before.
        format!("{}  shared_core.md\n", hex::encode(Sha256::digest(b"x"))),
        format!("{}  shared_core.md\n", hex.to_uppercase()),
        format!("{hex}  shared_core.md"),
        format!("{hex}  shared_core.md\n\n"),
        format!("{hex}  shared_core.md\r\n"),
        format!("{hex} shared_core.md\n"),
        format!("{hex}  docs/agents/shared_core.md\n"),
        format!("{hex}\n"),
        String::new(),
    ] {
        assert_eq!(
            hash_matches(copy, &bad),
            Err(format!(
                "{HASH} is {bad:?}, not {good:?} (run `sha256sum shared_core.md` in docs/agents)"
            )),
            "{bad:?}"
        );
    }
}

/// Catches CLAUDE.md losing its import: only a line that is exactly `@AGENTS.md`, outside
/// a fenced code block, counts.
#[test]
fn claude_md_imports_agents_md_only_by_a_line_of_its_own_outside_a_fence() {
    for good in [
        "@AGENTS.md\n",
        "@AGENTS.md",
        "# Notes\n\ntierstore specifics\n\n@AGENTS.md\n",
        "```\ncode\n```\n@AGENTS.md\n",
        "~~~~\n```\n~~~~\n@AGENTS.md\n",
        "````\n```\n````\n@AGENTS.md\n",
        "```\n```  \n@AGENTS.md\n",
    ] {
        assert!(imports_agents(good), "{good:?}");
    }
    for bad in [
        "",
        "\n",
        "AGENTS.md\n",
        "@AGENTS.md \n",
        " @AGENTS.md\n",
        "@AGENTS.md\r\n",
        "@agents.md\n",
        "read @AGENTS.md first\n",
        "@AGENTS.md.bak\n",
        "```\n@AGENTS.md\n```\n",
        "~~~\n@AGENTS.md\n~~~\n",
        "```md\n@AGENTS.md\n```\n",
        // A shorter or different fence does not close the open one.
        "````\n```\n@AGENTS.md\n````\n",
        "```\n~~~\n@AGENTS.md\n```\n",
        // A fence line with text after it does not close.
        "```\n``` x\n@AGENTS.md\n",
        // A fence never closed holds the rest of the file.
        "```\n@AGENTS.md\n",
    ] {
        assert!(!imports_agents(bad), "{bad:?}");
    }
}

/// Catches a fence misread: four spaces of indent (an indented code block), a run of two,
/// or backticks followed by a backtick is not a fence, so an import after it counts; up to
/// three spaces of indent is one.
#[test]
fn a_fence_is_indented_at_most_three_spaces() {
    assert!(imports_agents("    ```\n@AGENTS.md\n"));
    assert!(!imports_agents("   ```\n@AGENTS.md\n"));
    // Two backticks are not a fence.
    assert!(imports_agents("``\n@AGENTS.md\n"));
    // Nor is a run of backticks with a backtick after it, which is inline code; a tilde
    // fence's info string may hold one.
    assert!(imports_agents("```a`b\n@AGENTS.md\n"));
    assert!(!imports_agents("~~~a`b\n@AGENTS.md\n"));
}
