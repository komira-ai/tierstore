use std::io::Cursor;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{Answer, CycleCounts, Go, WholeCounts, serve, share};
use crate::list::{Entry, FileList};

fn entry(path: &str, size: u64) -> Entry {
    Entry {
        path: path.to_owned(),
        size,
    }
}

/// A tree of four files (one empty) and its list.
fn tree(dir: &Path) -> FileList {
    std::fs::create_dir_all(dir.join("d")).unwrap();
    std::fs::write(dir.join("a"), vec![1u8; 300_000]).unwrap();
    std::fs::write(dir.join("d/b"), vec![2u8; 1000]).unwrap();
    std::fs::write(dir.join("d/c"), b"").unwrap();
    std::fs::write(dir.join("e"), vec![3u8; 200_000]).unwrap();
    FileList {
        files: vec![
            entry("a", 300_000),
            entry("d/b", 1000),
            entry("d/c", 0),
            entry("e", 200_000),
        ],
        sha256: String::new(),
    }
}

/// The answers of a reader given `goes`, after its ready.
fn answers(list: &FileList, tree: &Path, goes: &[Go]) -> Vec<Answer> {
    let input: String = goes
        .iter()
        .map(|go| serde_json::to_string(go).unwrap() + "\n")
        .collect();
    let mut output = Vec::new();
    serve(list, tree, Cursor::new(input), &mut output).unwrap();
    let mut answers: Vec<Answer> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(answers.remove(0), Answer::Ready);
    answers
}

fn cycles_of(answer: &Answer) -> &CycleCounts {
    match answer {
        Answer::Cycles(counts) => counts,
        other => panic!("not cycles: {other:?}"),
    }
}

/// The shares of four readers split the list between them, each file in one share; a reader
/// told it is one of one gets every file, and `of` 0 is taken as one.
#[test]
fn the_shares_of_the_readers_split_the_list() {
    let files: Vec<Entry> = (0..10).map(|i| entry(&format!("f{i}"), 1)).collect();
    let names = |index, of| -> Vec<String> {
        share(&files, index, of)
            .into_iter()
            .map(|e| e.path.clone())
            .collect()
    };
    assert_eq!(names(0, 4), ["f0", "f4", "f8"]);
    assert_eq!(names(3, 4), ["f3", "f7"]);
    assert_eq!(names(0, 1).len(), 10);
    assert_eq!(names(0, 0).len(), 10);
    assert!(names(10, 11).is_empty());
}

/// A reader makes exactly the cycles it is allowed over its share, each with the four calls
/// timed and no error; a second phase starts its counts again.
#[test]
fn a_reader_makes_its_cycles_and_times_each_call() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-reader-cycles");
    let list = tree(dir.path());
    let go = |max_cycles, index, of| Go::Cycles {
        index,
        of,
        ns: u64::MAX,
        max_cycles,
        read_bytes: 4096,
    };
    let got = answers(&list, dir.path(), &[go(10, 0, 1), go(3, 1, 2)]);
    for (answer, want) in got.iter().zip([10, 3]) {
        let counts = cycles_of(answer);
        assert_eq!(
            (counts.count, counts.errors, &counts.first_error),
            (want, 0, &None)
        );
        for hist in [&counts.stat, &counts.open, &counts.read, &counts.close] {
            assert_eq!(hist.count, want);
            assert!(!hist.buckets.is_empty());
        }
    }
}

/// With no limit on the cycles, the time ends them (none when no time is given); a reader
/// whose share is empty makes none and does not wait.
#[test]
fn cycles_end_with_their_time_and_an_empty_share_makes_none() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-reader-time");
    let list = tree(dir.path());
    let got = answers(
        &list,
        dir.path(),
        &[
            Go::Cycles {
                index: 0,
                of: 1,
                ns: 0,
                max_cycles: 0,
                read_bytes: 1,
            },
            Go::Cycles {
                index: 7,
                of: 8,
                ns: u64::MAX,
                max_cycles: 0,
                read_bytes: 1,
            },
        ],
    );
    assert_eq!(cycles_of(&got[0]), &CycleCounts::default());
    assert_eq!(cycles_of(&got[1]), &CycleCounts::default());
}

/// A cycle that fails counts as an error and keeps no latency: a file that is not there fails
/// its `stat`, a socket its `open`, a directory its `read`; the first error is kept with its
/// call and path.
#[test]
fn a_failed_call_counts_the_cycle_as_an_error_and_keeps_the_first() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-reader-errors");
    let _socket = std::os::unix::net::UnixListener::bind(dir.path().join("sock")).unwrap();
    std::fs::create_dir(dir.path().join("dir")).unwrap();
    let list = FileList {
        files: vec![entry("dir", 0), entry("gone", 1), entry("sock", 0)],
        sha256: String::new(),
    };
    let got = answers(
        &list,
        dir.path(),
        &[Go::Cycles {
            index: 0,
            of: 1,
            ns: u64::MAX,
            max_cycles: 6,
            read_bytes: 8,
        }],
    );
    let counts = cycles_of(&got[0]);
    assert_eq!((counts.count, counts.errors), (0, 6));
    assert_eq!(
        counts.first_error.as_deref(),
        Some(
            format!(
                "read {}: Is a directory (os error 21)",
                dir.path().join("dir").display()
            )
            .as_str()
        )
    );
    assert_eq!(
        counts.stat.count + counts.open.count + counts.read.count + counts.close.count,
        0
    );
    let one = |name: &str| FileList {
        files: vec![entry(name, 1)],
        sha256: String::new(),
    };
    let go = [Go::Cycles {
        index: 0,
        of: 1,
        ns: u64::MAX,
        max_cycles: 1,
        read_bytes: 8,
    }];
    let gone = answers(&one("gone"), dir.path(), &go);
    assert!(
        cycles_of(&gone[0])
            .first_error
            .as_deref()
            .unwrap()
            .starts_with("stat ")
    );
    let sock = answers(&one("sock"), dir.path(), &go);
    assert!(
        cycles_of(&sock[0])
            .first_error
            .as_deref()
            .unwrap()
            .starts_with("open ")
    );
}

/// The whole-file reads take the largest files up to the budget, split between the readers,
/// and read every byte of each, cold or warm; a file that cannot be opened or read is an
/// error and the others are still read.
#[test]
fn whole_file_reads_read_every_byte_of_the_largest_files() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-reader-whole");
    let list = tree(dir.path());
    let whole = |index, of, budget, cold| Go::Whole {
        index,
        of,
        budget,
        cold,
    };
    let got = answers(
        &list,
        dir.path(),
        &[
            whole(0, 1, 1 << 30, true),
            whole(0, 1, 1 << 30, false),
            whole(0, 2, 300_001, true),
            whole(1, 2, 300_001, true),
        ],
    );
    let want = |files, bytes| {
        Answer::Whole(WholeCounts {
            files,
            bytes,
            errors: 0,
        })
    };
    assert_eq!(
        got,
        [
            want(3, 501_000),
            want(3, 501_000),
            want(1, 300_000),
            want(1, 200_000)
        ]
    );
    let _socket = std::os::unix::net::UnixListener::bind(dir.path().join("sock")).unwrap();
    std::fs::create_dir(dir.path().join("dir")).unwrap();
    let odd = FileList {
        files: vec![entry("a", 300_000), entry("dir", 5), entry("sock", 4)],
        sha256: String::new(),
    };
    let got = answers(&odd, dir.path(), &[whole(0, 1, 1 << 30, true)]);
    assert_eq!(
        got,
        [Answer::Whole(WholeCounts {
            files: 1,
            bytes: 300_000,
            errors: 2
        })]
    );
}

/// A line that is not a go ends the reader with an error that quotes it; an empty input ends
/// it after its ready.
#[test]
fn a_line_that_is_not_a_go_is_refused_and_an_empty_input_ends_the_reader() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-reader-bad");
    let list = tree(dir.path());
    let mut output = Vec::new();
    let error = serve(
        &list,
        dir.path(),
        Cursor::new("{\"phase\":\"dance\"}\n"),
        &mut output,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("reader: not a go: {\"phase\":\"dance\"}: "),
        "{error}"
    );
    assert!(answers(&list, dir.path(), &[]).is_empty());
}

/// A writer that refuses every write.
struct Broken;

impl std::io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("refused"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A reader whose answers cannot be written ends with an error that says so.
#[test]
fn a_reader_that_cannot_answer_ends_with_an_error() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-reader-broken");
    let list = tree(dir.path());
    let error = serve(&list, dir.path(), Cursor::new(""), Broken).unwrap_err();
    assert_eq!(error.to_string(), "reader: write the answer: refused");
}
