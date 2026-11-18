use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{Entry, FileList, HEADER, read, render, sha256, walk, write};
use crate::Error;

fn entry(path: &str, size: u64) -> Entry {
    Entry {
        path: path.to_owned(),
        size,
    }
}

/// A tree of nested directories, an empty file and a symbolic link.
fn tree(dir: &Path) {
    std::fs::create_dir_all(dir.join("include/sys")).unwrap();
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("include/a.h"), b"int a;").unwrap();
    std::fs::write(dir.join("include/sys/b.h"), vec![7u8; 5000]).unwrap();
    std::fs::write(dir.join("lib/empty.c"), b"").unwrap();
    std::fs::write(dir.join("top.txt"), b"top").unwrap();
    std::os::unix::fs::symlink("top.txt", dir.join("link")).unwrap();
}

/// A walk lists every regular file below the root, by its `/`-joined path and size, sorted by
/// path, and leaves the link and the directories out; the list written and read back is the
/// same, with the SHA-256 of the file's bytes.
#[test]
fn a_tree_is_listed_by_path_and_size_and_reads_back_the_same() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-list");
    let root = dir.path().join("tree");
    tree(&root);
    let out = dir.path().join("list");
    let written = write(&root, &out).unwrap();
    let want = vec![
        entry("include/a.h", 6),
        entry("include/sys/b.h", 5000),
        entry("lib/empty.c", 0),
        entry("top.txt", 3),
    ];
    assert_eq!(written.files, want);
    let text = std::fs::read(&out).unwrap();
    assert_eq!(
        String::from_utf8(text.clone()).unwrap(),
        format!("{HEADER}\n6\tinclude/a.h\n5000\tinclude/sys/b.h\n0\tlib/empty.c\n3\ttop.txt\n")
    );
    assert_eq!(written.sha256, sha256(&text));
    let back = read(&out).unwrap();
    assert_eq!(back, written);
    assert_eq!(back.bytes(), 5009);
}

/// The largest files, largest first and by path among equal sizes, until the budget is reached
/// (the file that reaches it included); never an empty file; all of them when the budget is
/// larger than the list.
#[test]
fn the_largest_files_are_taken_until_the_budget_is_reached() {
    let list = FileList {
        files: vec![
            entry("a", 10),
            entry("b", 30),
            entry("c", 0),
            entry("d", 30),
            entry("e", 5),
        ],
        sha256: String::new(),
    };
    let names =
        |budget| -> Vec<String> { list.largest(budget).into_iter().map(|e| e.path).collect() };
    assert_eq!(names(30), ["b"]);
    assert_eq!(names(31), ["b", "d"]);
    assert_eq!(names(1000), ["b", "d", "a", "e"]);
    assert!(names(0).is_empty());
}

/// Each defect of a list file is refused with its line and what is wrong.
#[test]
fn a_list_that_is_not_of_the_format_is_refused_with_its_line() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-list-bad");
    let cases: &[(&[u8], usize, &str)] = &[
        (
            b"# other 1\n",
            1,
            "the first line is not `# fsbench list 1`",
        ),
        (b"", 1, "the first line is not `# fsbench list 1`"),
        (b"# fsbench list 1\n\xff\n", 1, "the list is not UTF-8"),
        (
            b"# fsbench list 1\n12 a\n",
            2,
            "no tab between the size and the path",
        ),
        (
            b"# fsbench list 1\nx\ta\n",
            2,
            "the size is not a number of bytes",
        ),
        (
            b"# fsbench list 1\n1\ta//b\n",
            2,
            "the path has an empty, `.` or `..` component",
        ),
        (
            b"# fsbench list 1\n1\t../b\n",
            2,
            "the path has an empty, `.` or `..` component",
        ),
        (
            b"# fsbench list 1\n1\t/b\n",
            2,
            "the path has an empty, `.` or `..` component",
        ),
        (
            b"# fsbench list 1\n1\ta/./b\n",
            2,
            "the path has an empty, `.` or `..` component",
        ),
        (
            b"# fsbench list 1\n1\tb\n1\ta\n",
            3,
            "the paths are not sorted, or one is listed twice",
        ),
        (
            b"# fsbench list 1\n1\ta\n1\ta\n",
            3,
            "the paths are not sorted, or one is listed twice",
        ),
    ];
    for (i, (text, line, why)) in cases.iter().enumerate() {
        let path = dir.path().join(format!("list-{i}"));
        std::fs::write(&path, text).unwrap();
        let error = read(&path).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("{}:{line}: {why}", path.display()),
            "case {i}"
        );
    }
    let missing = dir.path().join("missing");
    assert!(matches!(
        read(&missing),
        Err(Error::Io { what: "read", .. })
    ));
}

/// A name that is not UTF-8, or that holds a tab or a newline, cannot be listed, and the walk
/// says which; a root that is not there is an error that names it.
#[test]
fn a_name_that_cannot_be_listed_and_a_missing_root_are_errors() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-list-names");
    let latin = dir.path().join("latin");
    std::fs::create_dir(&latin).unwrap();
    std::fs::write(latin.join(OsStr::from_bytes(b"caf\xe9")), b"").unwrap();
    let error = walk(&latin).unwrap_err().to_string();
    assert!(error.ends_with(": the name is not UTF-8"), "{error}");
    for name in ["a\tb", "a\nb"] {
        let odd = dir.path().join(format!("odd-{}", name.len()));
        std::fs::create_dir_all(odd.join("sub")).unwrap();
        std::fs::write(odd.join("sub").join(name), b"").unwrap();
        let error = walk(&odd).unwrap_err().to_string();
        assert!(
            error.ends_with(": a name with a tab or a newline cannot be listed"),
            "{error}"
        );
    }
    let missing = dir.path().join("missing");
    let error = write(&missing, &dir.path().join("out")).unwrap_err();
    assert!(
        matches!(
            error,
            Error::Io {
                what: "read the directory",
                ..
            }
        ),
        "{error}"
    );
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    let unwritable = dir.path().join("no-such-dir/out");
    let error = write(&dir.path().join("empty"), &unwritable).unwrap_err();
    assert!(matches!(error, Error::Io { what: "write", .. }), "{error}");
}

/// The text of a list is its header and one line per file, in the order given.
#[test]
fn render_writes_the_header_and_a_line_per_file() {
    assert_eq!(render(&[]), format!("{HEADER}\n"));
    assert_eq!(
        render(&[entry("x/y", 1), entry("z", 22)]),
        format!("{HEADER}\n1\tx/y\n22\tz\n")
    );
}
