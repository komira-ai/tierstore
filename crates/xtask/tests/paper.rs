//! The completion check of the tierstore paper.
//!
//! The paper is written with places that wait for a measurement or a decision. This test says
//! when none is left. It reads `tests/paper_fixture/tierstore.html`, a copy of the paper (the
//! paper itself is not in a repository yet; refresh the copy from the published page, and when
//! the paper moves here this path is the only thing that changes), and holds it to these rules:
//!
//! - no `class="tbd"` span, no `[to measure]` and no `[to decide]` anywhere in the text;
//! - no `data-blocked` attribute: a cell that carries a reason is a cell that is still waiting;
//! - every `data-cell` element carries `data-result`, the name of a file in
//!   `tests/paper_fixture/results/` that holds the measurement, or `data-basis` (`design`,
//!   `code-reading` or `decision`) when the cell states what the design is or how the code
//!   works and claims no measurement;
//! - every number printed inside an element with `data-result` is a value of its result file
//!   (the `value` column, third of the tab-separated columns; lines starting with `#` are
//!   comments). A number that no result holds is a number made up or mistyped.
//!
//! `the_paper_is_complete` runs all four. It fails today: the paper is not complete. It stays
//! red until the last cell is filled, so it is `#[ignore]`d and runs in its own workflow
//! (`.github/workflows/paper.yml`) with `--include-ignored`, which does not belong to the
//! checks `ci.yml` and `release.yml` require.
//! `the_measured_cells_trace_to_their_results` is the last rule alone, over the cells that are
//! filled, and runs with every test. The other tests plant each defect in a small page and show
//! the check refusing it.
//!
//! What the trace does not check: that a derived value (the `derived` rows) was computed as its
//! last column says. The rows name the arithmetic; nothing here redoes it.

use std::path::{Path, PathBuf};

const BASES: &[&str] = &["design", "code-reading", "decision"];
const VOID: &[&str] = &[
    "br", "hr", "img", "meta", "link", "input", "col", "wbr", "source",
];

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/paper_fixture")
}

/// One start or end tag of the page.
struct Tag<'a> {
    name: &'a str,
    attrs: &'a str,
    /// Byte offset of the `<`.
    start: usize,
    /// Byte offset just after the `>`.
    end: usize,
    closing: bool,
    open: bool,
}

/// Every tag of `html`. A `<` that is not followed by a letter or `/` is text. The end of a tag
/// is the first `>` outside quotes.
fn tags(html: &str) -> Vec<Tag<'_>> {
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied().unwrap_or(b' ');
        if !(next.is_ascii_alphabetic() || next == b'/') {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut quote = 0u8;
        while j < bytes.len() {
            let c = bytes[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner = &html[i + 1..j];
        let closing = inner.starts_with('/');
        let inner = inner.trim_start_matches('/');
        let name_end = inner
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(inner.len());
        let name = &inner[..name_end];
        let attrs = &inner[name_end..];
        let open = !closing && !attrs.trim_end().ends_with('/') && !VOID.contains(&name);
        out.push(Tag {
            name,
            attrs,
            start: i,
            end: j + 1,
            closing,
            open,
        });
        i = j + 1;
    }
    out
}

/// The value of attribute `name` in the attribute text of a tag.
fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let mut from = 0;
    while let Some(found) = attrs[from..].find(&needle) {
        let at = from + found;
        if at == 0 || attrs.as_bytes()[at - 1].is_ascii_whitespace() {
            let rest = &attrs[at + needle.len()..];
            return rest.find('"').map(|end| &rest[..end]);
        }
        from = at + needle.len();
    }
    None
}

/// The 1-based line of a byte offset.
fn line_of(html: &str, offset: usize) -> usize {
    1 + html.as_bytes()[..offset]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
}

/// The text inside the element whose start tag is `tags[k]`: its tags and entities removed.
/// `None` when the element is never closed.
fn element_text(html: &str, tags: &[Tag<'_>], k: usize) -> Option<String> {
    let first = &tags[k];
    let mut depth = 1usize;
    for tag in &tags[k + 1..] {
        if tag.name != first.name {
            continue;
        }
        if tag.closing {
            depth -= 1;
            if depth == 0 {
                return Some(plain(&html[first.end..tag.start]));
            }
        } else if tag.open {
            depth += 1;
        }
    }
    None
}

/// `inner` without its tags and without `&name;` and `&#n;` entities.
fn plain(inner: &str) -> String {
    let mut out = String::new();
    let mut rest = inner;
    while let Some(at) = rest.find('<') {
        let after = rest.as_bytes().get(at + 1).copied().unwrap_or(b' ');
        if after.is_ascii_alphabetic() || after == b'/' {
            out.push_str(&rest[..at]);
            match rest[at..].find('>') {
                Some(end) => rest = &rest[at + end + 1..],
                None => return strip_entities(&out),
            }
        } else {
            out.push_str(&rest[..=at]);
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    strip_entities(&out)
}

fn strip_entities(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        let tail = &rest[at + 1..];
        let end = tail.find(';');
        let is_entity = end.is_some_and(|e| {
            (1..=8).contains(&e)
                && tail[..e]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '#')
        });
        out.push_str(&rest[..at]);
        match (is_entity, end) {
            (true, Some(e)) => rest = &tail[e + 1..],
            _ => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The numbers printed in `text`. A number starts at a digit that does not follow a letter, a
/// digit or a point (so `p50`, `S3`, `SigV4` and `x86_64` hold none), and is digits with an
/// optional fraction; a comma followed by three digits is a thousands separator.
fn numbers(text: &str) -> Vec<f64> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i].is_ascii_digit()
            && (i == 0
                || !(chars[i - 1].is_ascii_alphanumeric()
                    || chars[i - 1] == '.'
                    || chars[i - 1] == '_'));
        if !starts {
            i += 1;
            continue;
        }
        let mut digits = String::new();
        while i < chars.len() {
            let c = chars[i];
            let digit_at = |k: usize| chars.get(k).is_some_and(char::is_ascii_digit);
            if c.is_ascii_digit() || (c == '.' && digit_at(i + 1)) {
                digits.push(c);
                i += 1;
            } else if c == ','
                && digit_at(i + 1)
                && digit_at(i + 2)
                && digit_at(i + 3)
                && !digit_at(i + 4)
            {
                i += 1;
            } else {
                break;
            }
        }
        out.push(digits.parse().expect("digits with one fraction parse"));
    }
    out
}

/// The `value` column of a result file. Lines starting with `#` and blank lines are skipped.
fn result_values(text: &str) -> Result<Vec<f64>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let value = line
            .split('\t')
            .nth(2)
            .ok_or_else(|| format!("line {}: fewer than 3 tab-separated columns", n + 1))?;
        out.push(
            value
                .parse::<f64>()
                .map_err(|_| format!("line {}: value {value:?} is not a number", n + 1))?,
        );
    }
    Ok(out)
}

/// Whether `name` is a plain file name of a result: lower-case letters, digits, `.`, `-` and
/// `_`, ending in `.tsv`, no `..`.
fn result_name_ok(name: &str) -> bool {
    !name.contains("..")
        && name.ends_with(".tsv")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// One line per number, in a cell with `data-result`, that its result file does not hold, per
/// result name that is unusable, and per cell that is never closed.
fn trace_problems(html: &str, results: &Path) -> Vec<String> {
    let all = tags(html);
    let mut problems = Vec::new();
    for (k, tag) in all.iter().enumerate() {
        let Some(name) = attr(tag.attrs, "data-result") else {
            continue;
        };
        let line = line_of(html, tag.start);
        let Some(cell) = attr(tag.attrs, "data-cell") else {
            problems.push(format!(
                "line {line}: data-result {name} on an element with no data-cell"
            ));
            continue;
        };
        if !result_name_ok(name) {
            problems.push(format!(
                "line {line}: {cell}: data-result {name:?} is not a plain .tsv name"
            ));
            continue;
        }
        let values = match std::fs::read_to_string(results.join(name)) {
            Ok(text) => match result_values(&text) {
                Ok(values) => values,
                Err(why) => {
                    problems.push(format!("line {line}: {cell}: {name}: {why}"));
                    continue;
                }
            },
            Err(_) => {
                problems.push(format!(
                    "line {line}: {cell}: result file {name} does not exist"
                ));
                continue;
            }
        };
        let Some(text) = element_text(html, &all, k) else {
            problems.push(format!("line {line}: {cell}: the element is never closed"));
            continue;
        };
        for number in numbers(&text) {
            if !values.iter().any(|v| (v - number).abs() < 1e-9) {
                problems.push(format!(
                    "line {line}: {cell}: the number {number} is not a value of {name}"
                ));
            }
        }
    }
    problems
}

/// One line per place that is still open.
fn open_places(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    for tag in tags(html).iter().filter(|t| !t.closing) {
        let line = line_of(html, tag.start);
        let class = attr(tag.attrs, "class").unwrap_or("");
        if class.split_whitespace().any(|c| c == "tbd") {
            found.push(format!("line {line}: a class=\"tbd\" span"));
        }
        if let Some(why) = attr(tag.attrs, "data-blocked") {
            found.push(format!("line {line}: data-blocked ({why})"));
        }
        if let Some(cell) = attr(tag.attrs, "data-cell") {
            let basis = attr(tag.attrs, "data-basis");
            let has_result = attr(tag.attrs, "data-result").is_some();
            let has_basis = basis.is_some_and(|b| BASES.contains(&b));
            if !has_result && !has_basis {
                found.push(format!(
                    "line {line}: cell {cell} has neither data-result nor data-basis"
                ));
            }
        }
    }
    for (n, text) in html.lines().enumerate() {
        for marker in ["[to measure]", "[to decide]"] {
            for _ in text.matches(marker) {
                found.push(format!("line {}: {marker}", n + 1));
            }
        }
    }
    found
}

/// The problems of both rules, as one report; empty when the paper is complete.
fn report(html: &str, results: &Path) -> String {
    let open = open_places(html);
    let trace = trace_problems(html, results);
    let mut text = format!(
        "{} open places and {} untraced numbers\n",
        open.len(),
        trace.len()
    );
    for group in [&open, &trace] {
        for item in group.iter().take(12) {
            text.push_str(item);
            text.push('\n');
        }
        if group.len() > 12 {
            text.push_str(&format!("... and {} more\n", group.len() - 12));
        }
    }
    if open.is_empty() && trace.is_empty() {
        String::new()
    } else {
        text
    }
}

fn paper() -> String {
    std::fs::read_to_string(fixture().join("tierstore.html")).expect("the paper fixture")
}

#[test]
#[ignore = "the paper is not complete yet; runs in .github/workflows/paper.yml with --include-ignored, and loses this attribute with the last cell"]
fn the_paper_is_complete() {
    // Catches: any cell of the paper still waiting for its measurement or its decision, and
    // any number in a measured cell that no result holds.
    let text = report(&paper(), &fixture().join("results"));
    assert!(text.is_empty(), "the paper is not complete: {text}");
}

#[test]
fn the_measured_cells_trace_to_their_results() {
    // Catches: a number edited in a filled cell without its result file, a data-result that
    // names a missing file, and a filled cell that lost its element. Holds today.
    let html = paper();
    let problems = trace_problems(&html, &fixture().join("results"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    let measured = tags(&html)
        .iter()
        .filter(|t| attr(t.attrs, "data-result").is_some())
        .count();
    assert!(
        measured >= 4,
        "the paper has {measured} measured cells, not the 4 filled so far"
    );
}

#[test]
fn each_kind_of_open_place_is_counted() {
    // Catches: a check that misses one kind of open place: a tbd span among other classes, a
    // bracketed placeholder in svg text, a blocked reason, a cell with no result or basis.
    let page = "<p>a <span class=\"x tbd\">[to measure]</span></p>\n\
        <text>[to decide]</text>\n\
        <span data-cell=\"a.b\" data-blocked=\"later\">x</span>\n\
        <span data-cell=\"c.d\">y</span>\n\
        <span data-cell=\"e.f\" data-basis=\"design\">z</span>\n\
        <span data-cell=\"g.h\" data-basis=\"guess\">w</span>\n";
    let open = open_places(page);
    let joined = open.join("\n");
    assert!(joined.contains("line 1: a class=\"tbd\" span"), "{joined}");
    assert!(joined.contains("line 1: [to measure]"), "{joined}");
    assert!(joined.contains("line 2: [to decide]"), "{joined}");
    assert!(joined.contains("line 3: data-blocked (later)"), "{joined}");
    assert!(joined.contains("cell a.b has neither"), "{joined}");
    assert!(joined.contains("cell c.d has neither"), "{joined}");
    assert!(!joined.contains("cell e.f"), "{joined}");
    assert!(joined.contains("cell g.h has neither"), "{joined}");
    assert_eq!(open.len(), 7, "{joined}");
}

#[test]
fn a_page_with_every_cell_filled_and_traced_is_complete() {
    // Catches: a check that refuses a clean page (so it could never go green).
    let dir = scratch("clean");
    std::fs::write(
        dir.join("r.tsv"),
        "# c\nraw\tx\t1754\tms\tm\ta.b\nraw\ty\t0.70\tms\tm\ta.b\n",
    )
    .unwrap();
    let page = "<p><span data-cell=\"a.b\" data-result=\"r.tsv\">It took 1,754 ms and p50 of 0.7 ms (S3).</span> \
        <span data-cell=\"c.d\" data-basis=\"code-reading\">It retries 3 times.</span></p>";
    assert_eq!(report(page, &dir), "");
}

#[test]
fn a_number_no_result_holds_is_refused() {
    // Catches: a measured cell whose printed number was changed or never measured.
    let dir = scratch("number");
    std::fs::write(dir.join("r.tsv"), "raw\tx\t12.8\tms\tm\ta.b\n").unwrap();
    let page = "<p><span data-cell=\"a.b\" data-result=\"r.tsv\">12.8 ms, then 13.8 ms</span></p>";
    let problems = trace_problems(page, &dir);
    assert_eq!(
        problems,
        vec!["line 1: a.b: the number 13.8 is not a value of r.tsv".to_owned()]
    );
}

#[test]
fn an_unusable_result_name_or_file_is_refused() {
    // Catches: a data-result that climbs out of the results directory, names no file, names a
    // file whose rows have no number, or sits on an element with no cell id.
    let dir = scratch("names");
    std::fs::write(dir.join("bad.tsv"), "raw\tx\tmany\tms\tm\n").unwrap();
    std::fs::write(dir.join("short.tsv"), "raw\tx\n").unwrap();
    let page = "<span data-cell=\"a\" data-result=\"../r.tsv\">1</span>\n\
        <span data-cell=\"b\" data-result=\"gone.tsv\">1</span>\n\
        <span data-cell=\"c\" data-result=\"bad.tsv\">1</span>\n\
        <span data-cell=\"d\" data-result=\"short.tsv\">1</span>\n\
        <span data-result=\"r.tsv\">1</span>\n\
        <div data-cell=\"e\" data-result=\"bad.tsv\"><div>1</div>";
    let joined = trace_problems(page, &dir).join("\n");
    assert!(
        joined.contains("line 1: a: data-result \"../r.tsv\" is not a plain .tsv name"),
        "{joined}"
    );
    assert!(
        joined.contains("line 2: b: result file gone.tsv does not exist"),
        "{joined}"
    );
    assert!(
        joined.contains("line 3: c: bad.tsv: line 1: value \"many\" is not a number"),
        "{joined}"
    );
    assert!(
        joined.contains("line 4: d: short.tsv: line 1: fewer than 3 tab-separated columns"),
        "{joined}"
    );
    assert!(
        joined.contains("line 5: data-result r.tsv on an element with no data-cell"),
        "{joined}"
    );
    assert!(
        joined.contains("line 6: e: bad.tsv: line 1: value \"many\" is not a number"),
        "{joined}"
    );
}

#[test]
fn an_unclosed_measured_cell_is_refused() {
    // Catches: a cell whose element never closes, which would be read as holding no numbers.
    let dir = scratch("unclosed");
    std::fs::write(dir.join("r.tsv"), "raw\tx\t1\tms\tm\n").unwrap();
    let page = "<div data-cell=\"a\" data-result=\"r.tsv\"><div>1</div>";
    assert_eq!(
        trace_problems(page, &dir),
        vec!["line 1: a: the element is never closed".to_owned()]
    );
}

#[test]
fn numbers_are_read_as_the_paper_prints_them() {
    // Catches: a number scanner that reads a digit inside a word (p50, S3, SigV4, x86_64) or
    // splits 1,754, or misses a figure after a bracket, an entity or a percent sign.
    assert_eq!(
        numbers("p50 of 447 &micro;s (64 streams), S3, SigV4, x86_64, 1,754 ms, 0.70, 19%. 4"),
        vec![447.0, 64.0, 1754.0, 0.7, 19.0, 4.0]
    );
    assert_eq!(numbers("v0.0.41 and 10, 20"), vec![10.0, 20.0]);
    assert_eq!(
        numbers("took .5 ms, (.25), 1.5, a.5, x_.5, and 3"),
        vec![0.5, 0.25, 1.5, 3.0]
    );
    assert_eq!(numbers("1.2.3 and 4, then 10.0.0.1."), vec![4.0]);
    assert_eq!(
        plain("a &#39;b&#39; <b>9.7</b> &amp; 1 < 2 &x"),
        "a b 9.7  1 < 2 &x"
    );
}

#[test]
fn a_number_from_another_cells_row_is_refused() {
    // Catches: a cell that quotes the value of a row that belongs to a different cell, which a
    // match against the whole file accepts. A row may name several cells (the escape for a
    // cell that quotes another cell's value); a row that names no cell serves none.
    let dir = scratch("wrong-row");
    std::fs::write(
        dir.join("r.tsv"),
        "raw\tfirst\t12.8\tms\tm\ta.b\n\
         raw\tsecond\t40\tms\tm\tc.d\n\
         raw\tshared\t7\tms\tm\ta.b, c.d\n\
         raw\tnone\t99\tms\tm\n",
    )
    .unwrap();
    let page = "<p><span data-cell=\"a.b\" data-result=\"r.tsv\">12.8 ms and 7 ms</span>\n\
        <span data-cell=\"c.d\" data-result=\"r.tsv\">12.8 ms, 40 ms and 7 ms</span>\n\
        <span data-cell=\"e.f\" data-result=\"r.tsv\">99 ms</span></p>";
    assert_eq!(
        trace_problems(page, &dir),
        vec![
            "line 2: c.d: the number 12.8 is not a value of r.tsv for this cell; the rows holding it (first) do not name c.d".to_owned(),
            "line 3: e.f: the number 99 is not a value of r.tsv for this cell; the rows holding it (none) do not name e.f".to_owned(),
        ]
    );
}

#[test]
fn attributes_not_in_double_quotes_are_refused() {
    // Catches: a tbd class, a data-blocked reason or a cell id written in single quotes, with no
    // quotes or with no value, which a reader of double-quoted values never sees; and an
    // attribute name in capitals.
    let page = "<p class='tbd'>a</p>\n\
        <span data-cell='a.b' data-blocked='later'>x</span>\n\
        <span data-blocked>y</span>\n\
        <td colspan=2>z</td>\n\
        <p CLASS=\"tbd\">u</p>\n\
        <p class=\"fine\" hidden>ok</p>\n";
    let open = open_places(page);
    let joined = open.join("\n");
    assert!(
        joined.contains("line 1: attribute class is not double-quoted"),
        "{joined}"
    );
    assert!(
        joined.contains("line 2: attribute data-cell is not double-quoted"),
        "{joined}"
    );
    assert!(
        joined.contains("line 2: attribute data-blocked is not double-quoted"),
        "{joined}"
    );
    assert!(
        joined.contains("line 3: attribute data-blocked has no value"),
        "{joined}"
    );
    assert!(
        joined.contains("line 4: attribute colspan is not double-quoted"),
        "{joined}"
    );
    assert!(joined.contains("line 5: a class=\"tbd\" span"), "{joined}");
    assert!(!joined.contains("line 6"), "{joined}");
    assert_eq!(open.len(), 6, "{joined}");
}

#[test]
fn markers_are_found_in_any_case() {
    // Catches: a placeholder typed [To measure] or [TO DECIDE], which an exact match misses.
    let open = open_places("[To Measure] and [TO DECIDE]\n");
    assert_eq!(
        open,
        vec![
            "line 1: [to measure]".to_owned(),
            "line 1: [to decide]".to_owned()
        ]
    );
}

#[test]
fn a_dotted_version_in_a_measured_cell_is_refused() {
    // Catches: a token like 1.2.3 in a measured cell, which is no measurement and is neither
    // 1.2 nor 3 (a version is written with a leading v, which the scanner skips).
    let dir = scratch("version");
    std::fs::write(dir.join("r.tsv"), "raw\tx\t12.8\tms\tm\ta\n").unwrap();
    let page = "<span data-cell=\"a\" data-result=\"r.tsv\">release 1.2.3 took 12.8 ms, v0.0.41 too</span>";
    assert_eq!(
        trace_problems(page, &dir),
        vec!["line 1: a: 1.2.3 is a dotted version, not a number".to_owned()]
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("paper-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}
