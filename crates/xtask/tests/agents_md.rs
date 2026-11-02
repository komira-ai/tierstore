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

/// Whether `claude` has a line that is exactly `@AGENTS.md` outside a fenced code block and
/// outside a code span.
fn imports_agents(claude: &str) -> bool {
    // The open fence: its character and length; a closing fence is at least as long.
    let mut fence: Option<(char, usize)> = None;
    // The lines of the current run of non-blank lines outside a fence, where a code span
    // may reach from one line to the next.
    let mut block: Vec<&str> = Vec::new();
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
            (None, true) => {
                if imports_in_block(&block) {
                    return true;
                }
                block.clear();
                fence = ch.map(|c| (c, run));
            }
            (Some((c, n)), true)
                if ch == Some(c) && run >= n && t[run..].chars().all(|x| x == ' ' || x == '\t') =>
            {
                fence = None
            }
            (Some(_), _) => {}
            (None, false) if line.chars().all(|c| c == ' ' || c == '\t') => {
                if imports_in_block(&block) {
                    return true;
                }
                block.clear();
            }
            (None, false) => block.push(line),
        }
    }
    fence.is_none() && imports_in_block(&block)
}

/// Whether one of `block` (consecutive non-blank lines outside a fence) is exactly
/// `@AGENTS.md` and outside every code span.
fn imports_in_block(block: &[&str]) -> bool {
    let text = block.join("\n");
    let spans = code_spans(&text);
    let mut offset = 0;
    for line in block {
        let (s, e) = (offset, offset + line.len());
        if *line == IMPORT && !spans.iter().any(|&(a, b)| a <= s && e <= b) {
            return true;
        }
        offset = e + 1;
    }
    false
}

/// The code spans of `text`, as the byte ranges between their opening and closing runs of
/// backticks. Outside a span, a backtick after an odd number of backslashes is escaped:
/// it is literal, and the rest of its run may open a span. A span closes at the next run of
/// exactly its opener's length; inside a span a backslash is literal, so it escapes nothing.
/// An opener with no such closer is literal text.
fn code_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    // The end of the run of backticks that starts at `i`.
    let run_end = |i: usize| {
        (i..bytes.len())
            .find(|&j| bytes[j] != b'`')
            .unwrap_or(bytes.len())
    };
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let end = run_end(i);
        let slashes = bytes[..i].iter().rev().take_while(|&&b| b == b'\\').count();
        let start = if slashes % 2 == 1 { i + 1 } else { i };
        let len = end - start;
        // The first run after the opener with the opener's length.
        let mut j = end;
        let mut closer = None;
        while len > 0 && j < bytes.len() {
            if bytes[j] == b'`' {
                let e = run_end(j);
                if e - j == len {
                    closer = Some((j, e));
                    break;
                }
                j = e;
            } else {
                j += 1;
            }
        }
        match closer {
            Some((c, e)) => {
                spans.push((end, c));
                i = e;
            }
            None => i = end,
        }
    }
    spans
}

/// The headings of the shared core, and one rule of each, that the copy must hold.
const HEADINGS: [&str; 5] = [
    "### Tests and coverage",
    "### Design and change shape",
    "### CI and workflows",
    "### PR text and commits",
    "### Secrets and configuration",
];
const RULES: [&str; 5] = ["T1", "B8", "B9", "P4", "S1"];

/// Nothing when `copy` has every heading of [`HEADINGS`] as a line and a line starting
/// `- <id> ` for every id of [`RULES`]; otherwise one line per one that is missing.
fn has_core_sections(copy: &str) -> Result<(), Vec<String>> {
    let missing: Vec<String> = HEADINGS
        .iter()
        .filter(|h| !copy.lines().any(|l| l == **h))
        .map(|h| format!("{COPY} has no heading `{h}`"))
        .chain(
            RULES
                .iter()
                .filter(|id| !copy.lines().any(|l| l.starts_with(&format!("- {id} "))))
                .map(|id| format!("{COPY} has no rule {id}")),
        )
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
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
    assert_eq!(has_core_sections(&read(COPY)), Ok(()));
}

/// Catches a core and copy cut down together (with the hash updated to match): an empty
/// copy, a copy without one heading, and a copy without one rule are each refused, naming
/// what is missing.
#[test]
fn a_copy_without_a_heading_or_rule_of_the_shared_core_is_refused() {
    let full: String = HEADINGS
        .iter()
        .zip(RULES)
        .map(|(h, id)| format!("{h}\n\n- {id} BLOCK. text\n\n"))
        .collect();
    assert_eq!(has_core_sections(&full), Ok(()));
    let all: Vec<String> = HEADINGS
        .iter()
        .map(|h| format!("{COPY} has no heading `{h}`"))
        .chain(RULES.iter().map(|id| format!("{COPY} has no rule {id}")))
        .collect();
    assert_eq!(has_core_sections(""), Err(all));
    assert_eq!(
        has_core_sections(&full.replace("### CI and workflows\n", "")),
        Err(vec![format!(
            "{COPY} has no heading `### CI and workflows`"
        )])
    );
    // A heading kept only inside a longer line is not the heading.
    assert_eq!(
        has_core_sections(&full.replace("### CI and workflows\n", "#### CI and workflows\n")),
        Err(vec![format!(
            "{COPY} has no heading `### CI and workflows`"
        )])
    );
    assert_eq!(
        has_core_sections(&full.replace("- P4 BLOCK. text\n", "")),
        Err(vec![format!("{COPY} has no rule P4")])
    );
    // An id that only begins another id (`T1` in `T10`) is not the rule.
    assert_eq!(
        has_core_sections(&full.replace("- T1 BLOCK.", "- T10 BLOCK.")),
        Err(vec![format!("{COPY} has no rule T1")])
    );
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

/// Catches an import inside a code span that runs over several lines: Markdown reads no
/// import there. A span closes only at a run of backticks of its own length, and a run
/// with no closer, or one cut off by a blank line or a fence, is literal text.
#[test]
fn an_import_inside_a_multiline_code_span_is_not_read() {
    for bad in [
        "`\n@AGENTS.md\n`\n",
        "``\n@AGENTS.md\n``\n",
        "see `x\n@AGENTS.md\ny` here\n",
        // A run of another length does not close the span.
        "``\n`\n@AGENTS.md\n``\n",
        // A span closed before the fence that follows it.
        "`\n@AGENTS.md\n`\n```\nx\n```\n",
    ] {
        assert!(!imports_agents(bad), "{bad:?}");
    }
    for good in [
        // The span closed before the import.
        "`x`\n@AGENTS.md\n",
        "`\nx\n`\n@AGENTS.md\n",
        "``a`b``\n@AGENTS.md\n",
        // A run with no closer is literal text.
        "`\n@AGENTS.md\n",
        "``\n@AGENTS.md\n`\n",
        // A blank line ends the paragraph, so no span reaches across it.
        "`\n\n@AGENTS.md\n\n`\n",
        // A fence ends it too.
        "`\n```\nx\n```\n@AGENTS.md\n",
        // The import before the span opens.
        "@AGENTS.md\n`\nx\n`\n",
    ] {
        assert!(imports_agents(good), "{good:?}");
    }
}

/// Catches a fence closed by a line Markdown does not read as a closing fence: only spaces
/// and tabs may follow the closing run, and the run may be longer than the opening one.
#[test]
fn a_closing_fence_has_only_spaces_and_tabs_after_it() {
    for good in [
        "```\nx\n```\t\n@AGENTS.md\n",
        "```\nx\n``` \t \n@AGENTS.md\n",
        "```\nx\n``````\n@AGENTS.md\n",
        "~~~\nx\n~~~~~\n@AGENTS.md\n",
    ] {
        assert!(imports_agents(good), "{good:?}");
    }
    for bad in [
        "```\nx\n```\u{a0}\n@AGENTS.md\n",
        "```\nx\n```\u{2003}\n@AGENTS.md\n",
        "~~~\nx\n~~~\u{a0}\n@AGENTS.md\n",
        "```\nx\n```\r\n@AGENTS.md\n",
    ] {
        assert!(!imports_agents(bad), "{bad:?}");
    }
}

/// Catches an escaped backtick read as a span's opener, and a line of other white space
/// read as a blank line: either would close the span early and let an import inside it
/// count. Only spaces and tabs make a blank line, and a backslash inside a span escapes
/// nothing.
#[test]
fn escapes_and_blank_lines_are_read_as_markdown_reads_them() {
    for bad in [
        // The escaped backtick is literal; the next one opens a span around the import.
        "\\`\n`\n@AGENTS.md\n`\n",
        // Three backslashes: the last escapes the backtick.
        "\\\\\\`\n`\n@AGENTS.md\n`\n",
        // An escaped backtick, and the rest of its run opens a span.
        "\\``\n@AGENTS.md\n`\n",
        // Inside a span a backslash escapes nothing, so the backtick after it closes the
        // span; the next span holds the import.
        "`a\\`\n`\n@AGENTS.md\n`\n",
        // Lines of other white space do not end the paragraph.
        "`\n\u{a0}\n@AGENTS.md\n`\n",
        "`\n\u{2003}\n@AGENTS.md\n`\n",
    ] {
        assert!(!imports_agents(bad), "{bad:?}");
    }
    for good in [
        // Two backslashes are an escaped backslash; the backtick opens a span that closes
        // on the next line.
        "\\\\`\n`\n@AGENTS.md\n",
        // An escaped backtick with no span after it.
        "\\`\n@AGENTS.md\n",
        "\\`\n@AGENTS.md\n\\`\n",
        // A line of spaces and tabs is blank and ends the paragraph.
        "`\n \t \n@AGENTS.md\n`\n",
    ] {
        assert!(imports_agents(good), "{good:?}");
    }
}
