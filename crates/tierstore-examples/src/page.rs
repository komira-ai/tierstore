//! The page of an example (`page.md`) shows every file of the example whole: each in a fenced
//! code block whose info string has `file=<name>`, holding exactly the file's text. So what a
//! reader copies from the page is what the runner runs, with no line hidden.
//!
//! A block without `file=` is prose (a command to type, say) and is not compared. The check
//! fails on a file the page does not show, a file shown twice, a block of a file the example
//! does not have, a block that differs from its file, a code block that is not closed, and a
//! directory in the example (the page shows files only).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::{Error, io_at};
use crate::spec::PAGE_FILE;

/// Checks that the page of the example in `example` shows each of its files as it is.
///
/// # Errors
/// The directory or a file cannot be read, or the page differs ([`Error::Page`], every problem).
pub fn check(example: &Path) -> Result<(), Error> {
    let page_path = example.join(PAGE_FILE);
    let page = fs::read_to_string(&page_path).map_err(io_at(&page_path))?;
    let mut files = BTreeMap::new();
    let mut problems = Vec::new();
    for entry in fs::read_dir(example).map_err(io_at(example))? {
        let path = entry.map_err(io_at(example))?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            problems.push(format!("{name} is a directory, which the page cannot show"));
        } else if name != PAGE_FILE {
            files.insert(name, fs::read_to_string(&path).map_err(io_at(&path))?);
        }
    }
    problems.extend(page_problems(&page, &files));
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Page {
        page: page_path,
        problems,
    })
}

/// What differs between the code blocks of `page` and `files` (each file's text by its name).
#[must_use]
pub fn page_problems(page: &str, files: &BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut shown: BTreeMap<&str, usize> = BTreeMap::new();
    let (blocks, closed) = blocks(page);
    if !closed {
        problems.push("a code block is not closed".to_owned());
    }
    for (name, text) in &blocks {
        *shown.entry(*name).or_default() += 1;
        match files.get(*name) {
            None => problems.push(format!(
                "the page shows {name}, which is not a file of the example"
            )),
            Some(file) if file != text => problems.push(format!(
                "the page's {name} differs from the file at line {}",
                first_difference(file, text)
            )),
            Some(_) => {}
        }
    }
    for name in files.keys() {
        match shown.get(name.as_str()) {
            None => problems.push(format!("{name} is not on the page")),
            Some(1) => {}
            Some(n) => problems.push(format!("{name} is on the page {n} times")),
        }
    }
    problems
}

/// The code blocks of `page` that name a file, as (file, text), and whether every block was
/// closed. A block opens with a line of three or more backticks and its info string, and closes
/// with a line of at least as many backticks and nothing else.
fn blocks(page: &str) -> (Vec<(&str, String)>, bool) {
    let mut found = Vec::new();
    let mut open: Option<(usize, Option<&str>, String)> = None;
    for line in page.lines() {
        let ticks = line.len() - line.trim_start_matches('`').len();
        match open.take() {
            None if ticks >= 3 => {
                let file = line[ticks..]
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("file="));
                open = Some((ticks, file, String::new()));
            }
            None => {}
            Some((fence, file, text)) if ticks >= fence && line.trim_end().len() == ticks => {
                if let Some(file) = file {
                    found.push((file, text));
                }
            }
            Some((fence, file, mut text)) => {
                text.push_str(line);
                text.push('\n');
                open = Some((fence, file, text));
            }
        }
    }
    (found, open.is_none())
}

/// The first line (from 1) at which `a` and `b` differ.
fn first_difference(a: &str, b: &str) -> usize {
    let mut a_lines = a.split_inclusive('\n');
    let mut b_lines = b.split_inclusive('\n');
    let mut at = 1;
    while a_lines.next() == b_lines.next() {
        at += 1;
    }
    at
}
