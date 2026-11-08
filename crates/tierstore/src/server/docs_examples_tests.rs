//! The examples of the operator guide run through the real configuration loader.
//!
//! Every ```` ```toml ```` block of the landing page of the docs (`docs/index.md`) and of each
//! example (`docs/examples/*.md`) is a whole file, and must load as the start loads it, into the
//! shape the page says it is. `docs_tests` holds the same pages to the code for flags, keys and
//! links; this holds the files themselves, so an example cannot go stale when a rule of the loader
//! changes.

use std::path::Path;

use super::config::{Settings, settings_from_text};
use super::config_testing::DEFAULTS;

/// The shape of file a page shows.
#[derive(Debug, PartialEq)]
enum Shape {
    Storage,
    Cache,
    Combined,
}

/// The pages: file under `docs/`, how many TOML blocks it has, and the shape of each.
const PAGES: &[(&str, &[Shape])] = &[
    ("index.md", &[Shape::Storage]),
    ("examples/directory-as-bucket.md", &[Shape::Storage]),
    ("examples/cache-in-front-of-s3.md", &[Shape::Cache]),
    ("examples/cache-hot-cold.md", &[Shape::Combined]),
];

fn read(page: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join(page);
    std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()))
}

/// The bodies of the ```` ```toml ```` fences of `text` (a `toml fragment` is not a whole file).
fn toml_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        match (&mut open, line) {
            (None, "```toml") => open = Some(String::new()),
            (Some(_), "```") => blocks.extend(open.take()),
            (Some(body), _) => {
                body.push_str(line);
                body.push('\n');
            }
            (None, _) => {}
        }
    }
    blocks
}

fn shape_of(settings: &Settings) -> Shape {
    match settings {
        Settings::Storage(_) => Shape::Storage,
        Settings::Cache(_) => Shape::Cache,
        Settings::Combined(_) => Shape::Combined,
    }
}

/// Asserts that every TOML file of the docs landing page and of the three examples loads through
/// the loader into the shape its page names. Catches an example that a change of the loader (a
/// renamed key, a new rule, a shape refused) left behind.
#[test]
fn every_toml_file_of_the_docs_landing_page_and_the_examples_loads() {
    for (page, shapes) in PAGES {
        let blocks = toml_blocks(&read(page));
        assert_eq!(blocks.len(), shapes.len(), "{page}: the TOML files it shows");
        for (block, want) in blocks.iter().zip(*shapes) {
            let settings = settings_from_text(block, DEFAULTS)
                .unwrap_or_else(|why| panic!("{page}: the loader refuses the file: {why}"));
            assert_eq!(&shape_of(&settings), want, "{page}: the shape of the file");
        }
    }
}

/// Asserts that a block the loader refuses is found by this test's own check: a hot tier with no
/// budget (which every hot-tier example needs) and a file with one tier alone are both refused.
#[test]
fn a_file_the_loader_refuses_is_found() {
    let example = &toml_blocks(&read("examples/directory-as-bucket.md"))[0];
    let no_budget = example.replace("budget = \"200GiB\"\n", "");
    assert!(settings_from_text(&no_budget, DEFAULTS).is_err());
    let no_cold = example.replace("[cold_store]\nkind = \"dir\"\npath = \"/srv/big/tierstore\"\n", "");
    assert_ne!(&no_cold, example, "the cold table was in the file");
    assert!(settings_from_text(&no_cold, DEFAULTS).is_err());
}

/// Asserts that every example page has the parts the site promises: what it is for, a diagram, the
/// file, the commands, what to expect and the limits.
#[test]
fn every_example_has_a_diagram_a_file_commands_and_limits() {
    for (page, _) in PAGES.iter().skip(1) {
        let text = read(page);
        for heading in [
            "## What it is for",
            "## The file",
            "## Commands",
            "## What to expect",
            "## Limits",
        ] {
            assert!(text.contains(&format!("\n{heading}\n")), "{page}: no {heading}");
        }
        assert!(text.contains("\n```diagram\n"), "{page}: no diagram");
    }
}
