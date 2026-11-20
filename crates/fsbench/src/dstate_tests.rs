//! The sampler's text as the parse reads it: each kind of thread counted where it belongs, the
//! connections of `tierstore fs` alone summed, a tick cut by a stopped sampler left out, and
//! every line that is not what the sampler prints refused with its number.

use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{Error, NodeRun, Tick, load, parse, row, stack, waiting};

/// The build of the tests: from 10 s to 70 s (Unix nanoseconds).
const START: u64 = 10_000_000_000;
const END: u64 = 70_000_000_000;

fn parsed(text: &str) -> Result<NodeRun, Error> {
    parse(Path::new("node.txt"), text, ("run1", "n1"), (START, END))
}

/// The parse error of `text`: its line and why.
fn refused(text: &str) -> (usize, String) {
    match parsed(text) {
        Err(Error::Parse { line, why, .. }) => (line, why),
        other => panic!("not a parse error: {other:?}"),
    }
}

/// A head with the mount of `tierstore fs` on connection 519 and another FUSE mount on 77.
const HEAD: &str = "@node 7.0.0-42-generic 88\n#mounts\n0:519 tierstore-fs\n0:77 other-fs\n";

/// One tick holds every kind of thread; a job thread asleep outside FUSE (109) is only in the
/// count, and the stack line of a thread with no row (900) and the waiting count of the other
/// mount (77) are not counted.
#[test]
fn a_tick_counts_each_kind_of_thread_where_it_belongs() {
    let text = format!(
        "{HEAD}@tick 9000000000\n#load\n12.50 10.00 9.00 3/900 77\n#waiting\n\
         /sys/fs/fuse/connections/519/waiting:4\n/sys/fs/fuse/connections/77/waiting:100\n\
         #ps\n\
         101 R - job\n\
         102 Rl - job\n\
         103 D request_wait_answer job\n\
         104 S request_wait_answer job\n\
         105 D folio_wait_bit_common job\n\
         106 D - job\n\
         107 D - job\n\
         108 S - job\n\
         109 S - job\n\
         110 D - job\n\
         201 D folio_wait_bit_common -\n\
         202 D - -\n\
         203 D - srv\n\
         204 S request_wait_answer -\n\
         #count\n40 3\n\
         #stack\n\
         /proc/1/task/105/stack:[<0>] folio_wait_bit_common+0x1/0x2\n\
         /proc/1/task/105/stack:[<0>] fuse_file_read_iter+0x3/0x4\n\
         /proc/1/task/106/stack:[<0>] fuse_flush+0x3/0x4\n\
         /proc/1/task/107/stack:[<0>] __fuse_simple_request+0x3/0x4\n\
         /proc/1/task/108/stack:[<0>] fuse_file_read_iter+0x3/0x4\n\
         /proc/1/task/201/stack:[<0>] filemap_fault+0x3/0x4\n\
         /proc/1/task/203/stack:[<0>] fuse_dev_do_read+0x3/0x4\n\
         /proc/1/task/203/stack:[<0>] fuse_dev_read+0x3/0x4\n\
         /proc/1/task/900/stack:[<0>] request_wait_answer+0x3/0x4\n\
         @end 9500000000\n"
    );
    let run = parsed(&text).unwrap();
    assert_eq!(
        (run.run.as_str(), run.node.as_str(), run.kernel.as_str()),
        ("run1", "n1", "7.0.0-42-generic")
    );
    assert_eq!(
        (run.cpus, run.connections.clone(), run.build_ms),
        (88, vec![519], 60_000)
    );
    assert_eq!(
        run.ticks,
        vec![Tick {
            t_ms: -1000,
            load1: 12.5,
            waiting: Some(4),
            jobs: 3,
            job_threads: 40,
            // 101 and 102.
            job_running: 2,
            // 103, 105, 106, 107 and 110 (in D state outside FUSE).
            job_d: 5,
            // 104 (by its wchan) and 108 (by its stack).
            job_fuse_s: 2,
            // 103 and 104.
            job_fuse_request: 2,
            // 105 (a FUSE function and a folio wait).
            job_fuse_page: 1,
            // 106, 107 and 108 (a FUSE function and no folio wait).
            job_fuse_other: 3,
            // 103, 105, 106, 107, 110, 201, 202 and 203.
            d: 8,
            // 103, 105, 106 and 107: not the server's own thread 203.
            fuse_d: 4,
            // 201.
            d_fault: 1,
        }]
    );
    let tick = &run.ticks[0];
    assert_eq!((tick.job_active(), tick.job_fuse()), (9, 6));
}

/// Without `@end` the last tick may be cut, and is left out; a tick with no count, no listed
/// connection of `tierstore fs` and no rows counts nothing; blank lines are skipped.
#[test]
fn a_tick_cut_by_a_stopped_sampler_is_left_out() {
    let empty = "@tick 70000000000\n#load\n1.00 1.00 1.00 1/2 3\n\n#waiting\n#ps\n#stack\n";
    let cut = "@tick 71000000000\n#load\n2.00 1.00 1.00 1/2 3\n#ps\n101 R - job\n";
    let run = parsed(&format!("{HEAD}{empty}{cut}")).unwrap();
    assert_eq!(
        run.ticks,
        vec![Tick {
            t_ms: 60_000,
            load1: 1.0,
            ..Tick::default()
        }]
    );
    let ended = parsed(&format!("{HEAD}{empty}{cut}@end 72000000000\n")).unwrap();
    assert_eq!(ended.ticks.len(), 2);
    assert_eq!(ended.ticks[1].job_running, 1);
}

/// A connection is read only when its mount's source is `tierstore fs`: with none, `waiting` is
/// not known.
#[test]
fn waiting_is_none_when_no_mount_of_tierstore_fs_is_listed() {
    let text = "@node 7.0.0 4\n#mounts\n0:77 other-fs\n@tick 10000000000\n#waiting\n\
                /sys/fs/fuse/connections/77/waiting:5\n@end 10000000001\n";
    let run = parsed(text).unwrap();
    assert_eq!(
        (run.connections.clone(), run.ticks[0].waiting),
        (vec![], None)
    );
    assert_eq!(run.ticks[0].t_ms, 0);
}

/// Every line that is not what the sampler prints is refused, with its number and why.
#[test]
fn a_line_the_sampler_does_not_print_is_refused_with_its_number() {
    let tick = "@node 7.0.0 4\n@tick 1\n";
    let cases: Vec<(String, (usize, &str))> = vec![
        ("#bogus\n".into(), (1, "an unknown section `bogus`")),
        ("hello\n".into(), (1, "a line before any section")),
        (
            "@node 7.0.0 4\n#mounts\n0:5\n".into(),
            (3, "a mount wants a device and a source"),
        ),
        (
            "@node 7.0.0 4\n#mounts\n05 x\n".into(),
            (3, "not a device `major:minor`"),
        ),
        (
            "@node 7.0.0 4\n#mounts\n0:x x\n".into(),
            (3, "not a device `major:minor`"),
        ),
        (format!("{tick}#load\nhigh\n"), (4, "not a load average")),
        (format!("{tick}#load\n\t \n#load\n"), (0, "")),
        (
            format!("{tick}#waiting\nnope\n"),
            (4, "not a connection's waiting count"),
        ),
        (
            format!("{tick}#ps\n1 R -\n"),
            (4, "a ps row wants a tid, a state, a wchan and an owner"),
        ),
        (
            format!("{tick}#ps\nx R - job\n"),
            (4, "a ps row wants a tid, a state, a wchan and an owner"),
        ),
        (
            format!("{tick}#count\n1\n"),
            (4, "a count wants the jobs' threads and the jobs"),
        ),
        (
            format!("{tick}#count\n1 x\n"),
            (4, "a count wants the jobs' threads and the jobs"),
        ),
        (
            format!("{tick}#count\nx 1\n"),
            (4, "a count wants the jobs' threads and the jobs"),
        ),
        (
            format!("{tick}#count\n1 2 3\n"),
            (4, "a count wants the jobs' threads and the jobs"),
        ),
        (format!("{tick}#stack\nnope\n"), (4, "not a stack line")),
        (
            "@node 7.0.0 4\n#load\n1.0\n".into(),
            (3, "a sample outside a tick"),
        ),
        (
            "@node 7.0.0 4\n#waiting\n/sys/fs/fuse/connections/1/waiting:1\n".into(),
            (3, "a sample outside a tick"),
        ),
        (
            "@node 7.0.0 4\n#ps\n1 R - job\n".into(),
            (3, "a sample outside a tick"),
        ),
        (
            "@node 7.0.0 4\n#count\n1 1\n".into(),
            (3, "a sample outside a tick"),
        ),
        (
            "@node 7.0.0 4\n#stack\n/proc/1/task/1/stack:[<0>] fuse_open+0x1\n".into(),
            (3, "a sample outside a tick"),
        ),
        (
            format!("{tick}@end 2\n#load\n1.0\n"),
            (5, "a sample after `@end`"),
        ),
        (
            format!("{tick}@end 2\n@tick 3\n"),
            (4, "`@tick` after `@end`"),
        ),
        (
            format!("{tick}@end 2\n@end 3\n"),
            (4, "`@end` after `@end`"),
        ),
        ("@tock 1\n".into(), (1, "an unknown marker `@tock 1`")),
        (
            "@node 7.0.0\n".into(),
            (1, "an unknown marker `@node 7.0.0`"),
        ),
        ("@node 7.0.0 many\n".into(), (1, "not a count of CPUs")),
        (
            "@node 7.0.0 4\n@tick soon\n".into(),
            (2, "`@tick` wants a time"),
        ),
        ("#mounts\n".into(), (1, "no `@node` line")),
    ];
    for (text, (line, why)) in cases {
        if line == 0 {
            // Blank and whitespace lines are skipped wherever they are.
            assert!(parsed(&text).is_ok(), "{text:?}");
            continue;
        }
        assert_eq!(refused(&text), (line, why.to_owned()), "{text:?}");
    }
}

/// A build that ends before it starts is not one.
#[test]
fn a_build_that_ends_before_it_starts_is_refused() {
    let err = parse(Path::new("n.txt"), "@node 7 1\n", ("r", "n"), (END, START)).unwrap_err();
    assert_eq!(err.to_string(), "n.txt: the build ends before it starts");
}

/// The three line readers take what the sampler prints and nothing else.
#[test]
fn the_line_readers_take_the_sampler_s_forms_alone() {
    assert_eq!(
        waiting("/sys/fs/fuse/connections/519/waiting:12"),
        Some((519, 12))
    );
    for bad in [
        "/sys/fs/fuse/connections/519/max_background:12",
        "/sys/fs/fuse/connections/x/waiting:12",
        "/sys/fs/fuse/connections/519/waiting:many",
        "519/waiting:12",
    ] {
        assert_eq!(waiting(bad), None, "{bad}");
    }
    let (tid, r) = row("7 Ds request_wait_answer job").unwrap();
    assert_eq!(
        (tid, r.state, r.wchan.as_str(), r.job),
        (7, 'D', "request_wait_answer", true)
    );
    assert!(!row("7 S - srv").unwrap().1.job);
    assert_eq!(
        stack("/proc/1/task/2/stack:[<0>] fuse_open+0x91/0x300"),
        Some((2, "fuse_open"))
    );
    assert_eq!(
        stack("/proc/1/task/2/stack:[<0>] fuse_open"),
        Some((2, "fuse_open"))
    );
    for bad in [
        "/proc/1/task/2/stack:fuse_open+0x1",
        "/proc/1/task/x/stack:[<0>] fuse_open+0x1",
        "/proc/1/stack:[<0>] fuse_open+0x1",
        "/proc/1/task/2/wchan:[<0>] fuse_open+0x1",
        "proc/1/task/2/stack:[<0>] fuse_open+0x1",
    ] {
        assert_eq!(stack(bad), None, "{bad}");
    }
}

/// Writes `text` at `path`, compressed with zstd when the name ends in `.zst`.
fn put(path: &Path, text: &str) {
    let bytes = if path.extension().is_some_and(|e| e == "zst") {
        ruzstd::encoding::compress_to_vec(
            text.as_bytes(),
            ruzstd::encoding::CompressionLevel::Fastest,
        )
    } else {
        text.as_bytes().to_vec()
    };
    std::fs::write(path, bytes).unwrap();
}

/// The index names each run's nodes and their texts, compressed or not, relative to itself; the
/// runs come back in its order with their settings, every node parsed.
#[test]
fn the_index_loads_every_run_and_node_in_its_order() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-dstate-index");
    let node = |t: u64| format!("{HEAD}@tick {t}\n#count\n5 1\n@end {}\n", t + 1);
    put(&dir.path().join("a.txt.zst"), &node(START));
    put(&dir.path().join("b.txt"), &node(START + 1_000_000_000));
    let index = dir.path().join("runs.json");
    std::fs::write(
        &index,
        r#"{"runs": [
            {"run": "run1", "build_start_ns": 10000000000, "build_end_ns": 70000000000,
             "settings": {"targets": "//a:b"},
             "nodes": [{"node": "n1", "raw": "a.txt.zst"}, {"node": "n2", "raw": "b.txt"}]},
            {"run": "run2", "build_start_ns": 10000000000, "build_end_ns": 10000000000,
             "settings": {}, "nodes": []}
        ]}"#,
    )
    .unwrap();
    let runs = load(&index).unwrap();
    assert_eq!(runs.len(), 2);
    let (entry, nodes) = &runs[0];
    assert_eq!(
        (
            entry.run.as_str(),
            entry.settings.get("targets").map(String::as_str)
        ),
        ("run1", Some("//a:b"))
    );
    let seen: Vec<(&str, i64, u32)> = nodes
        .iter()
        .map(|n| (n.node.as_str(), n.ticks[0].t_ms, n.ticks[0].job_threads))
        .collect();
    assert_eq!(seen, vec![("n1", 0, 5), ("n2", 1000, 5)]);
    assert_eq!((runs[1].0.run.as_str(), runs[1].1.len()), ("run2", 0));
}

/// An index or a text that cannot be read or is not what it should be is refused, naming it.
#[test]
fn an_index_or_text_that_cannot_be_read_is_refused_naming_it() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-dstate-bad");
    let index = dir.path().join("runs.json");
    let message = |text: &str| -> String {
        std::fs::write(&index, text).unwrap();
        load(&index).unwrap_err().to_string()
    };
    assert!(
        message("{\"runs\": 1}").contains("not an index of runs"),
        "{}",
        message("{\"runs\": 1}")
    );
    assert!(message(r#"{"runs": [], "extra": 1}"#).contains("unknown field `extra`"));
    let one = |raw: &str| {
        format!(
            r#"{{"runs": [{{"run": "r", "build_start_ns": 1, "build_end_ns": 2, "settings": {{}},
                "nodes": [{{"node": "n", "raw": "{raw}"}}]}}]}}"#
        )
    };
    assert!(message(&one("missing.txt")).starts_with("read "));
    std::fs::write(dir.path().join("bad.zst"), b"not zstd at all").unwrap();
    assert!(message(&one("bad.zst")).starts_with("decompress "));
    let cut = ruzstd::encoding::compress_to_vec(
        "@node 7 1\n".repeat(1000).as_bytes(),
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    std::fs::write(dir.path().join("cut.zst"), &cut[..cut.len() / 2]).unwrap();
    assert!(message(&one("cut.zst")).starts_with("decompress "));
    std::fs::write(dir.path().join("latin.txt"), b"@node 7 1\n\xff\n").unwrap();
    assert!(message(&one("latin.txt")).starts_with("read "));
    std::fs::write(dir.path().join("wrong.txt"), "hello\n").unwrap();
    assert!(message(&one("wrong.txt")).ends_with("wrong.txt:1: a line before any section"));
    assert!(
        load(&dir.path().join("none.json"))
            .unwrap_err()
            .to_string()
            .starts_with("read ")
    );
}
