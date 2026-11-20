use std::cell::RefCell;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;

use tierstore_testkit::dir::TestDir;

use super::{
    DiskStat, Series, deltas, interval, parse_diskstats, parse_metrics, sample_disks, to_f64,
};
use crate::Error;
use crate::clock::{Clock, ManualClock};
use crate::record::DiskSample;

/// One line of `/proc/diskstats` for `name`, with the five counters the harness reads.
fn line(name: &str, reads: u64, sectors: u64, read_ms: u64, io_ms: u64, weighted: u64) -> String {
    format!(
        "   8       0 {name} {reads} 5 {sectors} {read_ms} 7 0 56 9 0 {io_ms} {weighted} 0 0 0 0 0 0\n"
    )
}

fn stat(reads: u64, sectors_read: u64, read_ms: u64, io_ms: u64, weighted_ms: u64) -> DiskStat {
    DiskStat {
        reads,
        sectors_read,
        read_ms,
        io_ms,
        weighted_ms,
    }
}

fn series(name: &str, labels: &str, value: f64) -> Series {
    Series {
        name: name.to_owned(),
        labels: labels.to_owned(),
        value,
    }
}

/// Each device of the text is found by name with its five counters; a line of fewer than 14
/// fields (an older kernel's) is left out, and so is a line where any of the five counters is
/// not a number.
#[test]
fn diskstats_lines_become_devices_and_bad_lines_are_left_out() {
    let mut text = line("sda", 100, 2048, 300, 400, 500);
    text.push_str(&line("loop2", 1, 8, 2, 3, 4));
    text.push_str("   8       1 sda1 1 2 3\n\n");
    let got = parse_diskstats(&text);
    assert_eq!(got.len(), 2);
    assert_eq!(got["sda"], stat(100, 2048, 300, 400, 500));
    assert_eq!(got["loop2"], stat(1, 8, 2, 3, 4));
    for field in [3, 5, 6, 12, 13] {
        let good = line("sdb", 1, 2, 3, 4, 5);
        let mut parts: Vec<&str> = good.split_whitespace().collect();
        parts[field] = "x";
        assert!(
            parse_diskstats(&parts.join(" ")).is_empty(),
            "field {field}"
        );
    }
}

/// An interval's figures: 2000 sectors in 1000 ms is 1000 KB/s; 200 ms over 50 reads is a 4 ms
/// wait; 1500 weighted ms in 1000 ms is a queue of 1.5; 500 busy ms in 1000 is 50 percent.
#[test]
fn an_interval_gives_the_rates_of_the_change() {
    let a = stat(100, 1000, 300, 100, 200);
    let b = stat(150, 3000, 500, 600, 1700);
    assert_eq!(
        interval("r", "n1", "sda", &a, &b, 5000, 1000),
        DiskSample {
            run: "r".into(),
            node: "n1".into(),
            device: "sda".into(),
            t_ms: 5000,
            reads: 50,
            rkb_s: 1000.0,
            r_await_ms: 4.0,
            aqu_sz: 1.5,
            util_pct: 50.0,
        }
    );
}

/// No read gives a wait of 0; a window of 0 ms gives rates of 0; a busy time longer than the
/// window is capped at 100 percent; a counter that went back counts as no change.
#[test]
fn an_interval_handles_no_reads_no_time_overload_and_counters_that_went_back() {
    let a = stat(10, 10, 10, 10, 10);
    let idle = interval("r", "n", "d", &a, &a, 1, 1000);
    assert_eq!(
        (idle.reads, idle.r_await_ms, idle.rkb_s, idle.util_pct),
        (0, 0.0, 0.0, 0.0)
    );
    let b = stat(20, 20, 30, 20, 50);
    let still = interval("r", "n", "d", &a, &b, 1, 0);
    assert_eq!(
        (still.reads, still.r_await_ms, still.rkb_s, still.aqu_sz),
        (10, 2.0, 0.0, 0.0)
    );
    assert_eq!(still.util_pct, 0.0);
    let busy = interval("r", "n", "d", &a, &stat(10, 10, 10, 3000, 10), 1, 1000);
    assert_eq!(busy.util_pct, 100.0);
    let back = interval("r", "n", "d", &a, &stat(0, 0, 0, 0, 0), 1, 1000);
    assert_eq!(
        (back.reads, back.rkb_s, back.aqu_sz, back.util_pct),
        (0, 0.0, 0.0, 0.0)
    );
    assert_eq!(to_f64(3), 3.0);
}

/// A page's series keep their labels as written, quoted values may hold spaces, commas, braces
/// and escaped quotes, a timestamp after the value is ignored, and comments, blank lines and
/// lines without a number, a name or a closed label set are skipped.
#[test]
fn metrics_pages_are_parsed_to_series() {
    let text = "\
# HELP gets Gets.
# TYPE gets_total counter
gets_total 7
gets_total{op=\"get\",node=\"a b, c}\"} 12.5 1700000000000
quote_total{v=\"say \\\"hi\\\" \\\\\"} 3
inf_value +Inf
nan_value NaN

  indented_total  4
no_value
bad_value abc
{orphan=\"x\"} 1
open{a=\"b\" 1
open_quote{a=\"b} 1
tab\t5
";
    let got = parse_metrics(text);
    assert_eq!(got.len(), 7);
    assert_eq!(got[0], series("gets_total", "", 7.0));
    assert_eq!(
        got[1],
        series("gets_total", "{op=\"get\",node=\"a b, c}\"}", 12.5)
    );
    assert_eq!(
        got[2],
        series("quote_total", "{v=\"say \\\"hi\\\" \\\\\"}", 3.0)
    );
    assert_eq!(got[3].name, "inf_value");
    assert!(got[3].value.is_infinite());
    assert!(got[4].value.is_nan());
    assert_eq!(got[5], series("indented_total", "", 4.0));
    assert_eq!(got[6], series("tab", "", 5.0));
}

/// Only counters and histogram parts that are in both readings and grew are reported, in the
/// order of the second reading: a gauge, a series only in one reading, a series that did not
/// change and one that went down (a restart) are left out; each series is told apart by its
/// labels.
#[test]
fn deltas_keep_the_counters_and_histogram_parts_that_grew() {
    let before = vec![
        series("a_total", "", 10.0),
        series("a_total", "{k=\"x\"}", 5.0),
        series("h_bucket", "{le=\"1\"}", 1.0),
        series("h_count", "", 2.0),
        series("h_sum", "", 1.5),
        series("gauge", "", 1.0),
        series("same_total", "", 4.0),
        series("restarted_total", "", 100.0),
        series("gone_total", "", 1.0),
    ];
    let after = vec![
        series("h_sum", "", 4.0),
        series("a_total", "{k=\"x\"}", 8.0),
        series("a_total", "", 11.0),
        series("h_bucket", "{le=\"1\"}", 3.0),
        series("h_count", "", 5.0),
        series("gauge", "", 9.0),
        series("same_total", "", 4.0),
        series("restarted_total", "", 3.0),
        series("new_total", "", 3.0),
    ];
    let got = deltas("run", "n2", &before, &after, 2500);
    let shown: Vec<(String, String, f64)> = got
        .iter()
        .map(|d| (d.name.clone(), d.labels.clone(), d.delta))
        .collect();
    assert_eq!(
        shown,
        vec![
            ("h_sum".to_owned(), String::new(), 2.5),
            ("a_total".to_owned(), "{k=\"x\"}".to_owned(), 3.0),
            ("a_total".to_owned(), String::new(), 1.0),
            ("h_bucket".to_owned(), "{le=\"1\"}".to_owned(), 2.0),
            ("h_count".to_owned(), String::new(), 3.0),
        ]
    );
    assert!(
        got.iter()
            .all(|d| d.run == "run" && d.node == "n2" && d.window_ms == 2500)
    );
}

/// The sampler's whole run over a file that the pause rewrites: with `count` 2 it reads the file
/// three times, calls the pause twice with the interval, and writes a line per device per
/// interval with the clock's `dt` and `t`.
#[tokio::test]
async fn the_sampler_writes_a_line_per_device_per_interval() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-sample");
    let file = dir.path().join("diskstats");
    let write = |sda: [u64; 5], loop2: [u64; 5]| {
        let mut text = line("sda", sda[0], sda[1], sda[2], sda[3], sda[4]);
        text.push_str(&line("sdb", 9, 9, 9, 9, 9));
        text.push_str(&line(
            "loop2", loop2[0], loop2[1], loop2[2], loop2[3], loop2[4],
        ));
        std::fs::write(&file, text).unwrap();
    };
    write([0, 0, 0, 0, 0], [0, 0, 0, 0, 0]);
    let clock = ManualClock::new(0);
    let calls = Rc::new(RefCell::new(Vec::new()));
    let step = Rc::new(RefCell::new(0_u64));
    let mut pause = {
        let (calls, step, clock, file) = (calls.clone(), step.clone(), clock.clone(), file.clone());
        move |ms: u64| {
            calls.borrow_mut().push(ms);
            let mut s = step.borrow_mut();
            *s += 1;
            clock.advance_us(if *s == 1 { 1_000_000 } else { 500_000 });
            let text = if *s == 1 {
                let mut t = line("sda", 10, 200, 40, 500, 1000);
                t.push_str(&line("sdb", 9, 9, 9, 9, 9));
                t.push_str(&line("loop2", 0, 0, 0, 0, 0));
                t
            } else {
                let mut t = line("sda", 10, 200, 40, 500, 1000);
                t.push_str(&line("sdb", 9, 9, 9, 9, 9));
                t.push_str(&line("loop2", 4, 80, 8, 100, 100));
                t
            };
            std::fs::write(&file, text).unwrap();
            Box::pin(async {}) as futures::future::BoxFuture<'static, ()>
        }
    };
    let mut out = Vec::new();
    let devices = vec!["sda".to_owned(), "loop2".to_owned()];
    let n = sample_disks(
        &file,
        &devices,
        "run1",
        "n3",
        1000,
        2,
        &*clock,
        &mut pause,
        &mut out,
        Path::new("samples.jsonl"),
    )
    .await
    .unwrap();
    assert_eq!(n, 4);
    assert_eq!(*calls.borrow(), vec![1000, 1000]);
    let samples: Vec<DiskSample> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let shown: Vec<(&str, u64, u64, f64, f64, f64)> = samples
        .iter()
        .map(|s| {
            (
                s.device.as_str(),
                s.t_ms,
                s.reads,
                s.r_await_ms,
                s.aqu_sz,
                s.util_pct,
            )
        })
        .collect();
    assert_eq!(
        shown,
        vec![
            ("loop2", 1000, 0, 0.0, 0.0, 0.0),
            ("sda", 1000, 10, 4.0, 1.0, 50.0),
            ("loop2", 1500, 4, 2.0, 0.2, 20.0),
            ("sda", 1500, 0, 0.0, 0.0, 0.0),
        ]
    );
    assert!(samples.iter().all(|s| s.run == "run1" && s.node == "n3"));
}

fn no_pause() -> impl FnMut(u64) -> futures::future::BoxFuture<'static, ()> {
    |_| Box::pin(async {})
}

/// A count of 0 reads the file once, writes nothing and never pauses; a device the file does
/// not have, and a file that is not there, are errors that name them.
#[tokio::test]
async fn the_sampler_names_a_missing_device_or_file() {
    let dir = TestDir::under(std::env::temp_dir(), "readbench-sample-missing");
    let file = dir.path().join("diskstats");
    std::fs::write(&file, line("sda", 1, 1, 1, 1, 1)).unwrap();
    let clock = ManualClock::new(0);
    let mut out = Vec::new();
    let sda = vec!["sda".to_owned()];
    let n = sample_disks(
        &file,
        &sda,
        "r",
        "n",
        10,
        0,
        &*clock,
        &mut |_| panic!("no pause expected"),
        &mut out,
        Path::new("o"),
    )
    .await
    .unwrap();
    assert_eq!((n, out.len()), (0, 0));

    let nvme = vec!["nvme0n1".to_owned()];
    let err = sample_disks(
        &file,
        &nvme,
        "r",
        "n",
        10,
        1,
        &*clock,
        &mut no_pause(),
        &mut out,
        Path::new("o"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("{}: no device nvme0n1 in the file", file.display())
    );

    let gone = dir.path().join("nothing");
    let err = sample_disks(
        &gone,
        &sda,
        "r",
        "n",
        10,
        1,
        &*clock,
        &mut no_pause(),
        &mut out,
        Path::new("o"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Io { what: "read", .. }));
}

/// A device that leaves the file between readings, and an output that cannot be written, end the
/// run with an error that names the file or the output.
#[tokio::test]
async fn the_sampler_stops_when_a_device_goes_or_the_output_fails() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let dir = TestDir::under(std::env::temp_dir(), "readbench-sample-stop");
    let file = dir.path().join("diskstats");
    std::fs::write(&file, line("sda", 1, 1, 1, 1, 1)).unwrap();
    let clock = ManualClock::new(0);
    let sda = vec!["sda".to_owned()];

    let mut removes = {
        let file = file.clone();
        move |_| {
            std::fs::write(&file, "").unwrap();
            Box::pin(async {}) as futures::future::BoxFuture<'static, ()>
        }
    };
    let err = sample_disks(
        &file,
        &sda,
        "r",
        "n",
        10,
        3,
        &*clock,
        &mut removes,
        &mut Vec::new(),
        Path::new("o"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Usage(_)));

    std::fs::write(&file, line("sda", 1, 1, 1, 1, 1)).unwrap();
    let err = sample_disks(
        &file,
        &sda,
        "r",
        "n",
        10,
        3,
        &*clock,
        &mut no_pause(),
        &mut Broken,
        Path::new("samples.jsonl"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "write samples.jsonl: disk full");
    assert_eq!(clock.now_us(), 0);
}
