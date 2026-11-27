//! The pages of the examples: each shows every file of its example whole, and the check refuses
//! each way a page can differ from its files.

use std::collections::BTreeMap;

use tierstore_examples::page::{check, page_problems};

use crate::{examples, scratch};

/// Asserts every example under `examples/` has a page that shows each of its files as it is, and
/// that there is at least one example.
#[test]
fn every_example_page_shows_its_files_as_they_are() {
    let mut seen = 0;
    for entry in std::fs::read_dir(examples()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            check(&path).unwrap_or_else(|e| panic!("{e}"));
            seen += 1;
        }
    }
    assert!(seen >= 1);
}

fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
        .collect()
}

/// Asserts the problems the check finds in a page: a file not shown, a file shown twice, a block
/// of a file that is not there, a block that differs (at its first differing line, also where
/// one text is the other's start), and a block that is not closed. A block without `file=` and a
/// longer fence are read as such.
#[test]
fn a_page_that_differs_from_its_files_is_refused_with_each_problem() {
    let ok = "intro\n```sh\nnot a file\n```\n````toml file=a.toml\nx = 1\n```\n````\n";
    assert_eq!(
        page_problems(ok, &files(&[("a.toml", "x = 1\n```\n")])),
        Vec::<String>::new()
    );

    let page = "```text file=a\none\ntwo\n```\n```text file=a\none\ntwo\n```\n\
                ```text file=b\nfirst\nsecond\n```\n```text file=gone\nx\n```\n\
                ```text file=d\nline\n```\n```text file=e\nopen\n";
    let have = files(&[
        ("a", "one\ntwo\n"),
        ("b", "first\nother\n"),
        ("c", "c\n"),
        ("d", "line\nmore\n"),
    ]);
    assert_eq!(
        page_problems(page, &have),
        [
            "a code block is not closed",
            "the page's b differs from the file at line 2",
            "the page shows gone, which is not a file of the example",
            "the page's d differs from the file at line 2",
            "a is on the page 2 times",
            "c is not on the page",
        ]
    );
}

/// Asserts the check of an example's directory: a directory inside it is a problem, and a
/// directory with no page is an error naming the page.
#[test]
fn an_example_directory_is_checked_as_a_whole() {
    let dir = scratch("page-dir");
    let example = dir.path();
    std::fs::write(example.join("page.md"), "```text file=a\na\n```\n").unwrap();
    std::fs::write(example.join("a"), "a\n").unwrap();
    assert!(check(example).is_ok());
    std::fs::create_dir(example.join("sub")).unwrap();
    assert_eq!(
        check(example).err().unwrap().to_string(),
        format!(
            "{}: sub is a directory, which the page cannot show",
            example.join("page.md").display()
        )
    );
    std::fs::remove_file(example.join("page.md")).unwrap();
    assert_eq!(
        check(example).err().unwrap().to_string(),
        format!(
            "{}: No such file or directory (os error 2)",
            example.join("page.md").display()
        )
    );
}
