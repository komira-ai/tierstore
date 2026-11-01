//! The verify commands of the install guide (`crates/tierstore/docs/install.md`) and the site's
//! install page (`crates/tierstore/site/content/install.html`), read for the tests that hold them
//! to what this crate checks a release with.

use std::path::Path;

/// The two install pages, each with its path from the repository root.
pub fn pages() -> [(&'static str, String); 2] {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    [
        "crates/tierstore/docs/install.md",
        "crates/tierstore/site/content/install.html",
    ]
    .map(|path| {
        let text = std::fs::read_to_string(root.join(path)).expect("an install page");
        (path, text)
    })
}

/// `page` without the markup of a code block (`div`, `pre` and `span` tags); every other `<`,
/// such as the `<commit>` of the guide, is kept.
fn unmarked(page: &str) -> String {
    let mut out = String::new();
    let mut rest = page;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let tag = &rest[at..];
        let end = tag.find('>').map_or(tag.len(), |end| end + 1);
        let name = tag[1..].trim_start_matches('/');
        let markup = ["div", "pre", "span"].iter().any(|n| name.starts_with(n));
        out.push_str(if markup { "" } else { &tag[..end] });
        rest = &tag[end..];
    }
    out.push_str(rest);
    out
}

/// The command lines of `page` that start with `start` and hold `has`: markup out, entities
/// decoded, continued lines joined, a leading `$ ` prompt dropped, words one space apart.
pub fn commands(page: &str, start: &str, has: &str) -> Vec<String> {
    unmarked(page)
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\\\n", " ")
        .lines()
        .map(|line| {
            let line = line.trim().trim_start_matches("$ ");
            line.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|line| line.starts_with(start) && line.contains(has))
        .collect()
}

/// The `--flag` words of a command line.
pub fn flags(line: &str) -> Vec<&str> {
    line.split(' ').filter(|w| w.starts_with("--")).collect()
}

/// The word after `flag` in a command line.
pub fn value_of<'a>(line: &'a str, flag: &str) -> &'a str {
    let words: Vec<&str> = line.split(' ').collect();
    let at = words
        .iter()
        .position(|w| *w == flag)
        .expect("the flag is in the line");
    words.get(at + 1).copied().expect("a value after the flag")
}

#[test]
fn a_command_is_read_from_a_code_block_of_either_page() {
    // Catches: a reader that keeps the markup or the prompt of a block, splits a continued line,
    // or drops a `<placeholder>` of the guide.
    let html = "<p>x</p>\n<div class=\"code\"><pre><span class=\"p\">$</span> gh a &lt;f&gt; \\\n    --b c</pre></div>";
    assert_eq!(commands(html, "gh a", "--b"), ["gh a <f> --b c"]);
    let md = "Run `gh a`:\n\n```sh\ngh a <f> \\\n  --b c\n```\n";
    assert_eq!(commands(md, "gh a", ""), ["gh a <f> --b c"]);
    assert_eq!(commands(md, "gh a", "--d"), Vec::<String>::new());
    assert_eq!(flags("gh a <f> --b c --d"), ["--b", "--d"]);
    assert_eq!(value_of("gh a <f> --b c --d", "--b"), "c");
}
