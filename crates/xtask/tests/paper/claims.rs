//! The page must not claim a build-time speedup or slowdown. The attempt-5 wall times are one run
//! per arm, the baseline is 9 h older and on an older kbf, the build is compute-bound and kbf's
//! own hot tier serves most repeat reads first, so they support no comparison of build time.
//!
//! The guard reads the text a reader sees (the page without its style and script, a space for
//! every tag, whitespace entities decoded), and on its own each attribute a reader is shown
//! (`content`, `aria-label`, `title`, `alt`, in any letter case). It splits that text into
//! sentences at `.`, `!` or `?` before white space, and each sentence into clauses at `;`, `:`
//! and joining words (but, however, yet, although, though, whereas), so a disclaimer covers only
//! its own proposition. A clause claims a build-time comparison when its sentence names a build and the clause:
//!
//! - has a word that compares speeds (faster, slower, speedup, ...), or
//! - has a word that compares amounts (fell, less, half, ...) while its sentence speaks of time
//!   (time, seconds, took, complete, ...), or
//! - prints, in such a sentence, a number within 10% of a comparison of the wall times, in its
//!   unit (their differences in seconds or minutes, their changes in percent, their ratios).
//!
//! A sentence about a build or a wall time that prints two of the wall times sets the runs side
//! by side and is refused, and a sentence about GETs at the object store may print only the
//! shares the run measured. A wall time alone (wall-clock, wall time) is the recorded runs' time,
//! so it is context enough for a speed word. The attempt-5 block's own text is left to the
//! attempt5 module, which holds it word for word; its tags' attributes are still read here.
//!
//! A clause that disclaims ("we make no claim", "claim no", "not evidence", "in either
//! direction") is not a claim, so the page can say plainly that it makes none. A sentence about
//! a build node, or about a build's inputs with no time in it, is not about build time. This is
//! a guard on wording, not a reader: it is held to the cases below.

use super::{Value, attributes, paper, strip_entities, tags};

/// Words that compare speeds; with a build named they are a claim on their own.
pub(super) const SPEED_WORDS: &[&str] = &[
    "faster",
    "slower",
    "quicker",
    "speedup",
    "speed-up",
    "speed up",
    "speeds up",
    "sped up",
    "slowdown",
    "slow-down",
    "slows down",
    "slowed down",
    "sooner",
    "shortened",
    "shortens",
    "lengthened",
    "lengthens",
    "snappier",
    "sluggish",
    "beat",
    "beats",
    "outperform",
    "outperforms",
    "outperformed",
    "better",
    "worse",
    "wins",
    "won",
    "improve",
    "improved",
    "improves",
    "improvement",
    "regressed",
    "regression",
];

/// Words that compare amounts; with a build named and time spoken of they are a claim.
const AMOUNT_WORDS: &[&str] = &[
    "less time",
    "more time",
    "shorter",
    "longer",
    "fell",
    "falls",
    "rose",
    "rises",
    "dropped",
    "drops",
    "increased",
    "increases",
    "decreased",
    "decreases",
    "reduced",
    "reduces",
    "cut",
    "cuts",
    "halved",
    "doubled",
    "half",
    "twice",
    "saved",
    "saves",
    "less",
    "lower",
    "higher",
    "earlier",
    "later",
    "more",
];

/// Words that say a sentence speaks of time.
const TIME_WORDS: &[&str] = &[
    "time",
    "wall",
    "wall-clock",
    "second",
    "seconds",
    "minute",
    "minutes",
    "hour",
    "hours",
    "duration",
    "took",
    "take",
    "takes",
    "complete",
    "completes",
    "completed",
    "finish",
    "finishes",
    "finished",
    "ran",
];

/// What a comparison of the attempt-5 wall times (6219, 6519, 6748 s) prints, with the unit it
/// is printed in: the differences in seconds or minutes, the changes in percent and the ratios
/// as times. A number within 10% of one, followed by its unit, counts.
const WALL_DERIVED: &[(&[f64], &[&str])] = &[
    (
        &[300.0, 529.0, 229.0],
        &["s", "sec", "secs", "second", "seconds"],
    ),
    // 300 s is 5 minutes, a figure too common to read as a comparison.
    (&[8.82, 3.82], &["min", "mins", "minute", "minutes"]),
    (&[4.82, 8.51, 3.51], &["%", "percent", "per cent"]),
    (
        &[1.048, 1.085, 1.035, 0.954, 0.922, 0.966],
        &["x", "\u{d7}", "times"],
    ),
];

/// The numbers of `clause` that a comparison of the wall times would print, with their unit.
fn derived_numbers(clause: &str) -> Vec<f64> {
    with_unit(clause)
        .into_iter()
        .filter(|(value, rest)| {
            WALL_DERIVED.iter().any(|(values, units)| {
                values.iter().any(|d| (d - value).abs() <= 0.1 * d) && starts_with_unit(rest, units)
            })
        })
        .map(|(value, _)| value)
        .collect()
}

/// Whether `rest` starts with one of `units` as a whole word.
fn starts_with_unit(rest: &str, units: &[&str]) -> bool {
    units.iter().any(|u| {
        rest.strip_prefix(u)
            .is_some_and(|tail| !tail.chars().next().is_some_and(char::is_alphanumeric))
    })
}

/// Each number printed in `clause`, with the text after it (white space trimmed).
fn with_unit(clause: &str) -> Vec<(f64, &str)> {
    let chars: Vec<(usize, char)> = clause.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        let after_word = i > 0 && (chars[i - 1].1.is_alphanumeric() || chars[i - 1].1 == '.');
        if !c.is_ascii_digit() || after_word {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && (chars[j].1.is_ascii_digit() || ",.".contains(chars[j].1)) {
            j += 1;
        }
        let end = chars.get(j).map_or(clause.len(), |c| c.0);
        let token = clause[at..end].trim_end_matches([',', '.']);
        let rest = clause[end..].trim_start();
        if let Some(&value) = super::numbers(token).first() {
            out.push((value, rest));
        }
        i = j;
    }
    out
}

/// Phrases with which a clause disclaims a comparison.
const DISCLAIMERS: &[&str] = &[
    "make no claim",
    "makes no claim",
    "claim no",
    "no claim",
    "draw no",
    "not evidence",
    "do not show",
    "does not show",
    "in either direction",
    "not a comparison",
];

/// Whether `phrase` occurs in `text` (both lower case) with no letter or digit on either side.
pub(super) fn has_word(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Whether a sentence names a build: build, builds or build-time, not as in a build node.
fn names_build(sentence: &str) -> bool {
    sentence.match_indices("build").any(|(at, _)| {
        let rest = &sentence[at + "build".len()..];
        let rest = rest.strip_prefix('s').unwrap_or(rest);
        let before = sentence[..at].chars().next_back();
        !before.is_some_and(char::is_alphanumeric)
            && !rest.chars().next().is_some_and(char::is_alphanumeric)
            && !rest.starts_with(" node")
            && !rest.starts_with("-node")
    })
}

/// `text` with the whitespace entities a browser renders as a space (`&nbsp;`, `&ensp;`,
/// `&emsp;`, `&thinsp;`, `&#160;`, `&#xa0;`) made one, and the rest handled as the parent
/// module's `strip_entities` does. Lower case, white space squeezed.
fn rendered(text: &str) -> String {
    let mut text = text.to_owned();
    for entity in [
        "&nbsp;", "&ensp;", "&emsp;", "&thinsp;", "&#160;", "&#xa0;", "&#xA0;",
    ] {
        text = text.replace(entity, " ");
    }
    let text = text.replace("&NBSP;", " ");
    strip_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The texts a reader is shown: the visible text of the page, then each shown attribute's text
/// on its own.
fn texts(html: &str) -> Vec<String> {
    let all = tags(html);
    let mut visible = String::new();
    let mut shown = Vec::new();
    let mut at = 0;
    // The attempt-5 block is held word for word by the attempt5 module (its table prints the
    // wall times side by side by design), so its visible text is left out here; the attributes
    // of its tags are still read.
    let mut hidden_until = 0;
    for (k, tag) in all.iter().enumerate() {
        // What looks like a tag inside a skipped style or script is not one.
        if tag.start < at {
            continue;
        }
        if tag.start >= hidden_until {
            visible.push_str(&html[at..tag.start]);
        }
        visible.push(' ');
        at = tag.end;
        if tag.closing {
            continue;
        }
        if super::attr(tag.attrs, "id") == Some(super::attempt5::BLOCK)
            && let Some(end) = super::element_end(&all, k)
        {
            hidden_until = end;
        }
        let name = tag.name.to_ascii_lowercase();
        if (name == "style" || name == "script")
            && let Some(end) = super::element_end(&all, k)
        {
            at = end;
        }
        for a in attributes(tag.attrs) {
            let shown_name = ["content", "aria-label", "title", "alt"]
                .iter()
                .any(|n| a.name.eq_ignore_ascii_case(n));
            if shown_name && let Value::Double(value) = a.value {
                shown.push(rendered(value));
            }
        }
    }
    visible.push_str(&html[at..]);
    let mut out = vec![rendered(&visible)];
    out.extend(shown);
    out
}

/// The sentences of `text`: it ends one at `.`, `!` or `?` before white space or at the end.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        if ".!?".contains(c) && bytes.get(i + 1).is_none_or(|b| *b == b' ') {
            out.push(text[start..i].trim());
            start = i + 1;
        }
    }
    out.push(text[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

/// Words that join two propositions. A disclaimer covers only the proposition it is in, so a
/// clause is cut at each of them as well as at `;` and `:`.
const JOINS: &[&str] = &["but", "however", "yet", "although", "though", "whereas"];

/// The clauses of a sentence: cut at `;`, `:` and before each word of `JOINS`.
fn clauses(sentence: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for part in sentence.split([';', ':']) {
        let mut cuts: Vec<usize> = JOINS
            .iter()
            .flat_map(|j| {
                part.match_indices(j)
                    .filter(|(at, _)| {
                        let before = part[..*at].chars().next_back();
                        let after = part[at + j.len()..].chars().next();
                        !before.is_some_and(char::is_alphanumeric)
                            && !after.is_some_and(char::is_alphanumeric)
                    })
                    .map(|(at, _)| at)
                    .collect::<Vec<_>>()
            })
            .collect();
        cuts.sort_unstable();
        let mut start = 0;
        for cut in cuts {
            out.push(&part[start..cut]);
            start = cut;
        }
        out.push(&part[start..]);
    }
    out
}

/// What claim, if any, the clause makes in its sentence.
fn claim(sentence: &str, clause: &str) -> Option<String> {
    // The wall times are the recorded build runs' times, so naming one is speaking of a build's
    // time even when the word build is absent.
    let wall = has_word(sentence, "wall") || sentence.contains("wall-clock");
    let build = names_build(sentence);
    if !(build || wall) || DISCLAIMERS.iter().any(|d| clause.contains(d)) {
        return None;
    }
    if let Some(word) = SPEED_WORDS.iter().find(|w| has_word(clause, w)) {
        return Some(format!("{word:?}"));
    }
    if !TIME_WORDS.iter().any(|w| has_word(sentence, w)) {
        return None;
    }
    // A word that compares amounts is a claim only of a build: "wall time fell" alone is said of
    // other runs on the page (a tree read at 1 and 64 requests in flight).
    if build && let Some(word) = AMOUNT_WORDS.iter().find(|w| has_word(clause, w)) {
        return Some(format!("{word:?}"));
    }
    derived_numbers(clause)
        .first()
        .map(|n| format!("{n}, a comparison of the wall times,"))
}

/// The attempt-5 wall times. Two of them in one sentence about a build or a wall time set the
/// runs side by side, which is a comparison whatever the words.
const WALL_TIMES: &[f64] = &[6219.0, 6519.0, 6748.0];

/// The shares a sentence about GETs at the object store may print: the cuts the run measured,
/// and its hit rates by object and by byte.
const GET_SHARES: &[f64] = &[42.0, 54.0, 41.4, 44.0, 13.7, 11.8];

/// One line per clause of the page that claims a build-time speedup or slowdown, and per share of
/// GETs at the object store that the run did not measure.
pub(super) fn time_claims(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    for text in texts(html) {
        for sentence in sentences(&text) {
            let context = names_build(sentence)
                || has_word(sentence, "wall")
                || sentence.contains("wall-clock");
            let walls: Vec<f64> = super::numbers(sentence)
                .into_iter()
                .filter(|n| WALL_TIMES.contains(n))
                .collect();
            if context && walls.iter().any(|w| walls.iter().any(|v| v != w)) {
                found.push(format!(
                    "{walls:?}, the wall times side by side, in a sentence about build time: {sentence}"
                ));
            }
            let gets = (has_word(sentence, "get") || has_word(sentence, "gets"))
                && (sentence.contains("minio") || sentence.contains("object store"));
            if gets {
                for (value, rest) in with_unit(sentence) {
                    if starts_with_unit(rest, &["%", "percent", "per cent"])
                        && !GET_SHARES.contains(&value)
                    {
                        found.push(format!(
                            "{value}%, a share of GETs the run did not measure: {sentence}"
                        ));
                    }
                }
            }
            for clause in clauses(sentence) {
                if let Some(what) = claim(sentence, clause) {
                    found.push(format!("{what} in a sentence about build time: {sentence}"));
                }
            }
        }
    }
    found
}

#[test]
fn the_page_claims_no_build_time_change() {
    // Catches: any sentence of the page, its shown attributes included, that turns the
    // attempt-5 wall times into a speedup or slowdown. Holds today.
    let claims = time_claims(&paper());
    assert!(claims.is_empty(), "{}", claims.join("\n"));
}

#[test]
fn a_build_time_claim_is_refused_in_every_form() {
    // Catches: a claim in words, in a comparison verb, as a derived number however rounded,
    // split by a colon, after a disclaimer in another clause, in a heading-like fragment, with
    // non-breaking spaces, in an attribute in capitals, and around a tag whose title ends a
    // sentence.
    for (page, says) in [
        ("<p>With tierstore the build ran faster.</p>", "\"faster\""),
        ("<p>Build time: faster with tierstore</p>", "\"faster\""),
        ("<p>Builds complete in half the time.</p>", "\"half\""),
        ("<p>Build time fell by 4.82%.</p>", "\"fell\""),
        (
            "<p>Build time went from 6,219 s to 6,748 s, 8.5% of it.</p>",
            "8.5, a comparison",
        ),
        (
            "<p>The cold build took 300 s extra.</p>",
            "300, a comparison",
        ),
        (
            "<p>We claim no slowdown: the build is faster.</p>",
            "\"faster\"",
        ),
        ("<p>The build&nbsp;is&nbsp;faster.</p>", "\"faster\""),
        ("<p>The build&#160;is&#160;faster.</p>", "\"faster\""),
        (
            "<meta CONTENT=\"tierstore gives a build speedup\">",
            "\"speedup\"",
        ),
        (
            "<svg ARIA-LABEL=\"Wall-clock regression of the build\"></svg>",
            "\"regression\"",
        ),
        // The original case: a wall time, no build named.
        (
            "<svg aria-label=\"Wall-clock regression of 4.8%\"></svg>",
            "\"regression\"",
        ),
        (
            "<p>Wall time rose by 8.5% with tierstore.</p>",
            "8.5, a comparison",
        ),
        // Forms found by trying to get past the guard.
        (
            "<p>Tierstore shortened the builds by five minutes.</p>",
            "\"shortened\"",
        ),
        (
            "<p>The build time was 8 percent lower with tierstore.</p>",
            "\"lower\"",
        ),
        (
            "<p>The build time was 8 percent of it.</p>",
            "8, a comparison",
        ),
        (
            "<p>The build finished 5 minutes earlier with tierstore.</p>",
            "\"earlier\"",
        ),
        ("<p>The build finished in 5 minutes less.</p>", "\"less\""),
        (
            "<p>The tierstore wall time of 6219 s beat the baseline 6748 s.</p>",
            "\"beat\"",
        ),
        (
            "<p>Tierstore wins on build time: 6219 vs 6748.</p>",
            "the wall times side by side",
        ),
        (
            "<p>Builds took 6219 s against 6748 s, so choose tierstore.</p>",
            "the wall times side by side",
        ),
        (
            "<p>The builds with tierstore got snappier than the baseline.</p>",
            "\"snappier\"",
        ),
        (
            "<p>Section 5 has the GETs that reached the cold MinIO, which fell by 99%.</p>",
            "99%, a share of GETs the run did not measure",
        ),
        (
            "<p>The wall time is slower with tierstore.</p>",
            "\"slower\"",
        ),
        // A disclaimer of something else does not cover the claim joined to it.
        (
            "<p>We make no claim about causation, but the build is faster with tierstore.</p>",
            "\"faster\"",
        ),
        (
            "<p>We make no claim about causation; however builds take less time.</p>",
            "\"less time\"",
        ),
        (
            "<p>The <span title=\"A note. Another\">build</span> is faster.</p>",
            "\"faster\"",
        ),
        (
            "<h3>Build time</h3><p>Faster with tierstore.</p>",
            "\"faster\"",
        ),
    ] {
        let joined = time_claims(page).join("\n");
        assert!(joined.contains(says), "{page}:\n{joined}");
    }
}

#[test]
fn a_disclaimer_or_an_unrelated_quantity_is_not_a_claim() {
    // Catches: a guard so broad that the page cannot say it makes no claim, or cannot speak of
    // a build's inputs, a build node or a time that is not a build's.
    for page in [
        "<p>We claim no build-time speedup.</p>",
        "<p>We make no claim about build time from these runs, in either direction.</p>",
        "<p>The wall times of the builds are not evidence about tierstore.</p>",
        "<p>This build uses a 300 MB input.</p>",
        "<p>A read on a build node is faster when it hits.</p>",
        "<p>At 64 in flight wall time fell 14 times while the median rose 3.5 times.</p>",
        "<p>Each distinct byte was read 111 to 133 times in one build.</p>",
        "<p>In a build, all 12 disks were 87 to 100% busy writing 0.9 MB/s; one upload took 5 minutes.</p>",
    ] {
        let claims = time_claims(page);
        assert!(claims.is_empty(), "{page}:\n{}", claims.join("\n"));
    }
}
