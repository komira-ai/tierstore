//! The C-10 arm: death scenarios, groupings, the summary, its tables and the verb end to end.

use clap::Parser as _;
use tierstore_testkit::dir::TestDir;

use super::*;

const MIB: u64 = 1 << 20;
/// An arrival time that is not on an hour (milliseconds since the epoch).
const T0: u64 = 1_790_000_000_000;

fn arrival(task_output: bool, dead: bool) -> Arrival {
    Arrival {
        arrival_ms: T0,
        size: MIB,
        writer_hash: 0,
        task_output,
        dead_on_arrival: dead,
    }
}

fn object(key: &str, size: u64) -> Object {
    Object {
        bucket: "cas-store".to_owned(),
        key: key.to_owned(),
        size,
        modified_ms: T0,
        etag: "e".to_owned(),
    }
}

/// The listing line of `o`.
fn line(o: &Object) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\n",
        o.bucket, o.key, o.size, o.modified_ms, o.etag
    )
}

/// The writer of a key is the first 8 bytes of blake3 of the key, little endian.
#[test]
fn the_writer_hash_is_the_first_eight_bytes_of_blake3_little_endian() {
    let hash = blake3::hash(b"cas/b/1-2/x/pack");
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    assert_eq!(writer_hash("cas/b/1-2/x/pack"), u64::from_le_bytes(first));
    assert_ne!(writer_hash("a"), writer_hash("b"));
}

/// Each scenario's rule: 7 days for all, 30 days for task outputs and 7 for the rest, and dead
/// keys at arrival.
#[test]
fn each_scenario_kills_objects_by_its_rule() {
    let task = arrival(true, false);
    let other = arrival(false, false);
    let dead = arrival(false, true);
    assert_eq!(Scenario::MinTtl.death_ms(&task), T0 + 7 * DAY_MS);
    assert_eq!(Scenario::MinTtl.death_ms(&dead), T0 + 7 * DAY_MS);
    assert_eq!(Scenario::TtlByKind.death_ms(&task), T0 + 30 * DAY_MS);
    assert_eq!(Scenario::TtlByKind.death_ms(&other), T0 + 7 * DAY_MS);
    assert_eq!(Scenario::TtlByKind.death_ms(&dead), T0 + 7 * DAY_MS);
    assert_eq!(Scenario::FooterDead.death_ms(&task), T0 + 30 * DAY_MS);
    assert_eq!(Scenario::FooterDead.death_ms(&other), T0 + 7 * DAY_MS);
    assert_eq!(Scenario::FooterDead.death_ms(&dead), T0);
}

/// Scenarios and groupings have names; arrival grouping is one class, expiry grouping the week
/// of the death modulo the classes.
#[test]
fn groupings_and_scenarios_have_names_and_classes() {
    assert_eq!(Scenario::MinTtl.name(), "min-ttl");
    assert_eq!(Scenario::TtlByKind.name(), "ttl-by-kind");
    assert_eq!(Scenario::FooterDead.name(), "footer-dead");
    assert_eq!(Grouping::Arrival.name(), "arrival");
    assert_eq!(Grouping::Expiry.name(), "expiry");
    assert_eq!(Grouping::Arrival.class(9 * 7 * DAY_MS, 8), 0);
    assert_eq!(Grouping::Expiry.class(9 * 7 * DAY_MS, 8), 1);
    assert_eq!(Grouping::Expiry.class(3 * 7 * DAY_MS + 5, 8), 3);
}

/// Objects without bytes are left out; a key with `cas/b/` is a task output; the dead keys are
/// matched by bucket and key.
#[test]
fn arrivals_leave_out_empty_objects_and_mark_task_outputs_and_dead_keys() {
    let objects = vec![
        object("cas/b/1-2/x/pack", 10),
        object("cas/d/n/b/0000000000000001", 20),
        object("cas/d/n/b/0000000000000002", 0),
    ];
    let mut dead = HashSet::new();
    dead.insert((
        "cas-store".to_owned(),
        "cas/d/n/b/0000000000000001".to_owned(),
    ));
    dead.insert(("other".to_owned(), "cas/b/1-2/x/pack".to_owned()));
    let a = arrivals(&objects, &dead);
    assert_eq!(a.len(), 2);
    assert_eq!(
        (a[0].size, a[0].task_output, a[0].dead_on_arrival),
        (10, true, false)
    );
    assert_eq!(
        (a[1].size, a[1].task_output, a[1].dead_on_arrival),
        (20, false, true)
    );
    assert_eq!(a[0].arrival_ms, T0);
    assert_eq!(a[0].writer_hash, writer_hash("cas/b/1-2/x/pack"));
}

/// The items carry the death of the scenario and the class of the grouping.
#[test]
fn items_carry_the_scenarios_death_and_the_groupings_class() {
    let a = [arrival(true, false), arrival(false, false)];
    let by_expiry = items(&a, Scenario::TtlByKind, Grouping::Expiry, 8);
    assert_eq!(by_expiry[0].death_ms, T0 + 30 * DAY_MS);
    assert_eq!(by_expiry[1].death_ms, T0 + 7 * DAY_MS);
    assert_eq!(by_expiry[0].class, expiry_class(T0 + 30 * DAY_MS, 8));
    assert_eq!(by_expiry[1].class, expiry_class(T0 + 7 * DAY_MS, 8));
    assert_ne!(by_expiry[0].class, by_expiry[1].class);
    assert_eq!(by_expiry[0].size, MIB);
    let by_arrival = items(&a, Scenario::MinTtl, Grouping::Arrival, 8);
    assert_eq!(by_arrival[0].class, 0);
    assert_eq!(by_arrival[1].class, 0);
}

/// The dead-keys file gives `(bucket, key)` per line, blank lines left out; a missing file and a
/// line without a tab are refused with the file and line.
#[test]
fn the_dead_keys_file_is_read_and_its_faults_named() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-gc-dead");
    let ok = dir.path().join("ok.tsv");
    std::fs::write(&ok, "b1\tk1\n\nb2\tk\twith tab\n").unwrap();
    let keys = read_dead_keys(&ok).unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&("b1".to_owned(), "k1".to_owned())));
    assert!(keys.contains(&("b2".to_owned(), "k\twith tab".to_owned())));

    let bad = dir.path().join("bad.tsv");
    std::fs::write(&bad, "b1\tk1\nnotab\n").unwrap();
    let e = read_dead_keys(&bad).unwrap_err();
    assert_eq!(
        e.to_string(),
        format!("{}:2: not `bucket TAB key`", bad.display())
    );

    let missing = dir.path().join("missing.tsv");
    assert!(matches!(
        read_dead_keys(&missing),
        Err(Error::Io { what: "read", .. })
    ));
}

/// The listing of the end-to-end tests: one 1 MiB pack of a task, one 1 MiB pack that the dead
/// keys name, and an empty object.
fn listing_text() -> (String, String) {
    let task = object("cas/b/1-2/x/pack", MIB);
    let dead = object("cas/d/n/b/0000000000000001", MIB);
    let empty = object("cas/d/n/b/0000000000000002", 0);
    (
        [task, dead.clone(), empty].iter().map(line).collect(),
        format!("{}\t{}\n", dead.bucket, dead.key),
    )
}

/// One object of 1 MiB at the design's values: under NOW it seals at 24 h, waits 72 h more and
/// makes a partial stripe (its record of 1 MiB and a 64-byte header, aligned, and two parity
/// shards as long), freed when the object dies; under LATER it seals at 72 h and is dead before
/// the 168 h of `partial_after` pass, so it is unlinked hot and never goes cold.
#[test]
fn one_object_follows_the_two_profiles_through_seal_partial_stripe_and_death() {
    let a = [arrival(false, false)];
    let s = summarize(&a, false);
    assert_eq!(s.runs.len(), 12);
    assert_eq!((s.objects, s.dead_on_arrival), (1, 0));
    assert_eq!((s.first_arrival_ms, s.last_arrival_ms), (T0, T0));
    let names: Vec<&str> = s.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["now", "now-replicas", "later"]);
    let labels: Vec<String> = s.runs.iter().map(Run::label).collect();
    assert_eq!(labels[0], "now/min-ttl/arrival");
    assert_eq!(labels[1], "now/min-ttl/expiry");
    assert_eq!(labels[2], "now/ttl-by-kind/arrival");
    assert_eq!(labels[7], "now-replicas/ttl-by-kind/expiry");
    assert_eq!(labels[11], "later/ttl-by-kind/expiry");

    let now = &s.runs[0].replay;
    assert_eq!(now.samples.len(), 61);
    assert_eq!(now.samples[3].partial_stripes, 0);
    let held = &now.samples[5];
    let record = MIB + 4096;
    assert_eq!(held.partial_stripes, 1);
    assert_eq!(held.cold_raw_bytes, 3 * record);
    assert_eq!(held.live_bytes, MIB);
    assert_eq!(held.cold_fill_efficiency, ratio(MIB, 3 * record));

    // Two cold copies: the block goes cold alone at twice its length, never as a partial stripe.
    let copies = &s.runs[4].replay.samples[5];
    assert_eq!(s.runs[4].label(), "now-replicas/min-ttl/arrival");
    assert_eq!((copies.stripes_live, copies.partial_stripes), (1, 0));
    assert_eq!(copies.cold_raw_bytes, 2 * record);
    assert_eq!(now.samples[7].stripes_freed, 1);
    assert_eq!(now.end_of_data.sample.stripes_live, 0);
    assert_eq!(now.end_of_replay.sample.stripes_freed, 1);
    assert_eq!(now.end_of_replay.sample.blocks_dropped_hot, 0);
    assert_eq!(now.end_of_replay.sealed_blocks.under_seal_min, 1);

    let later = &s.runs[8].replay;
    assert_eq!(s.runs[8].label(), "later/min-ttl/arrival");
    assert_eq!(later.end_of_replay.sample.blocks_dropped_hot, 1);
    assert_eq!(later.end_of_replay.sample.partial_stripes, 0);
    assert_eq!(later.end_of_replay.sample.stripes_freed, 0);
}

/// A task output lives 30 days under ttl-by-kind, so under LATER it is still alive when its
/// partial stripe forms (hour 240) and the stripe is freed at its death; a key the footer rule
/// found dead at arrival never goes cold under footer-dead.
#[test]
fn task_outputs_outlive_the_partial_wait_and_dead_keys_never_go_cold() {
    let a = [arrival(true, false), arrival(false, true)];
    let s = summarize(&a, true);
    assert_eq!(s.runs.len(), 18);
    assert_eq!(s.dead_on_arrival, 1);
    let find = |label: &str| s.runs.iter().find(|r| r.label() == label).unwrap();
    let ttl = &find("later/ttl-by-kind/arrival")
        .replay
        .end_of_replay
        .sample;
    assert_eq!(ttl.partial_stripes, 1);
    assert_eq!(ttl.stripes_freed, 1);
    assert_eq!(ttl.blocks_dropped_hot, 0);
    let footer = &find("now/footer-dead/expiry").replay.end_of_replay.sample;
    assert_eq!(footer.blocks_dropped_hot, 1);
    assert_eq!(footer.partial_stripes, 1);
}

/// The tables carry the `C-10` titles, one row per run and moment, and the figures of the run.
#[test]
fn the_tables_print_each_run_at_both_moments() {
    let s = summarize(&[arrival(false, false)], false);
    let t = tables(&s);
    let titles: Vec<String> = t
        .iter()
        .map(|t| t.markdown().lines().next().unwrap().to_owned())
        .collect();
    assert_eq!(
        titles,
        [
            "**C-10, cold tier at the end of the data and at the end of the replay**",
            "**C-10, cold_fill_efficiency % by week**",
            "**C-10, sealed blocks by size so far**",
            "**C-10, stripes not freed by live fraction**",
            "**C-10, rewrite cost below each rewrite_below**",
        ]
    );
    assert_eq!(t[0].rows().len(), 24);
    let md = t[0].markdown();
    assert!(md.contains(
        "| now | min-ttl | arrival | end of data | 0 B | 0 B | 0.0 | 0 | 0 | 0 | 0 | 0 | 0 B |\n"
    ));
    assert!(md.contains(
        "| now | min-ttl | arrival | end of replay | 0 B | 0 B | 0.0 | 0 | 1 | 1 | 0 | 0 | 0 B |\n"
    ));
    assert!(md.contains(
        "| later | ttl-by-kind | expiry | end of replay | 0 B | 0 B | 0.0 | 0 | 0 | 0 | 1 | 0 | 0 B |\n"
    ));
    // Weekly: days 0, 7, ..., 56 and a column per run; a held stripe reads 33.3.
    assert_eq!(t[1].rows().len(), 9);
    assert_eq!(t[1].rows()[0][0], "0");
    assert_eq!(t[1].rows()[8][0], "56");
    assert_eq!(t[1].rows()[0].len(), 1 + 12);
    // Sealed sizes: one block under seal_min_bytes in NOW at the end of the replay.
    assert!(
        t[2].markdown()
            .contains("| now | min-ttl | arrival | end of replay | 1 | 0 | 0 |\n")
    );
    // Deciles: ten bins; nothing is left in any.
    assert_eq!(t[3].rows().len(), 24);
    assert_eq!(t[3].rows()[0].len(), 4 + 10);
    // Rewrite: three thresholds per run and moment.
    assert_eq!(t[4].rows().len(), 72);
    assert_eq!(
        t[4].rows()[0],
        [
            "now",
            "min-ttl",
            "arrival",
            "end of data",
            "0.20",
            "0",
            "0 B",
            "0 B",
            "0.00",
            "0.00"
        ]
    );
}

/// A weekly table of runs with different numbers of samples leaves the short ones empty.
#[test]
fn the_weekly_table_leaves_empty_cells_for_a_run_with_fewer_days() {
    let mut s = summarize(&[arrival(false, false)], false);
    s.runs.truncate(2);
    s.runs[1].replay.samples.truncate(1);
    let w = &tables(&s)[1];
    assert_eq!(w.rows()[0], ["0", "0.0", "0.0"]);
    assert_eq!(w.rows()[1], ["7", "0.0", ""]);
}

/// The weekly table's efficiency is a percentage with one decimal.
#[test]
fn the_weekly_table_prints_the_efficiency_as_a_percentage() {
    let mut s = summarize(&[arrival(false, false)], false);
    s.runs.truncate(1);
    s.runs[0].replay.samples[7].cold_fill_efficiency = 1.0 / 3.0;
    assert_eq!(tables(&s)[1].rows()[1], ["7", "33.3"]);
}

/// Runs the verb on files of `dir` and returns what it printed.
fn run_on(dir: &TestDir, dead: Option<&str>) -> Result<String, Error> {
    let args = Args {
        listing: dir.path().join("listing.tsv"),
        dead_keys: dead.map(|d| dir.path().join(d)),
        out: dir.path().join("c10.json"),
    };
    let mut out = Vec::new();
    run(&args, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

/// `c10` reads the listing and the dead keys, prints the tables and writes a summary that reads
/// back to the replay of the same objects.
#[test]
fn c10_replays_a_listing_and_writes_a_summary_that_reads_back() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-gc-run");
    let (listing, dead) = listing_text();
    std::fs::write(dir.path().join("listing.tsv"), &listing).unwrap();
    std::fs::write(dir.path().join("dead.tsv"), &dead).unwrap();
    let printed = run_on(&dir, Some("dead.tsv")).unwrap();
    let json = std::fs::read_to_string(dir.path().join("c10.json")).unwrap();
    let summary: Summary = serde_json::from_str(&json).unwrap();
    assert_eq!((summary.objects, summary.dead_on_arrival), (2, 1));
    assert_eq!(summary.runs.len(), 18);
    let expected = summarize(
        &arrivals(
            &listing::read(&dir.path().join("listing.tsv")).unwrap(),
            &read_dead_keys(&dir.path().join("dead.tsv")).unwrap(),
        ),
        true,
    );
    assert_eq!(summary, expected);
    assert!(printed.starts_with("**C-10, cold tier at the end of the data"));
    assert_eq!(
        printed.lines().last().unwrap(),
        format!(
            "2 objects replayed in 18 runs; summary in {}",
            dir.path().join("c10.json").display()
        )
    );
}

/// Without `--dead-keys` there is no footer-dead scenario.
#[test]
fn c10_without_dead_keys_runs_two_scenarios() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-gc-nodead");
    std::fs::write(dir.path().join("listing.tsv"), listing_text().0).unwrap();
    let printed = run_on(&dir, None).unwrap();
    assert!(printed.ends_with(&format!(
        "2 objects replayed in 12 runs; summary in {}\n",
        dir.path().join("c10.json").display()
    )));
    assert!(!printed.contains("footer-dead"));
}

/// The verb refuses a missing listing, a listing with nothing to replay, a missing or malformed
/// dead-keys file and an output it cannot write.
#[test]
fn c10_refuses_what_it_cannot_read_or_write() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-gc-refuse");
    assert!(matches!(
        run_on(&dir, None),
        Err(Error::Io { what: "open", .. })
    ));

    std::fs::write(
        dir.path().join("listing.tsv"),
        line(&object("cas/d/n/b/0000000000000002", 0)),
    )
    .unwrap();
    let e = run_on(&dir, None).unwrap_err();
    assert_eq!(
        e.to_string(),
        format!(
            "{} has no object with bytes to replay",
            dir.path().join("listing.tsv").display()
        )
    );

    std::fs::write(dir.path().join("listing.tsv"), listing_text().0).unwrap();
    assert!(matches!(
        run_on(&dir, Some("nodead.tsv")),
        Err(Error::Io { what: "read", .. })
    ));
    std::fs::write(dir.path().join("bad.tsv"), "no tab\n").unwrap();
    assert!(matches!(
        run_on(&dir, Some("bad.tsv")),
        Err(Error::Parse { line: 1, .. })
    ));

    // The summary cannot be written where a directory is.
    std::fs::create_dir(dir.path().join("c10.json")).unwrap();
    assert!(matches!(
        run_on(&dir, None),
        Err(Error::Io { what: "write", .. })
    ));
}

/// A writer that takes `room` bytes and then fails.
struct Room {
    room: usize,
}

impl Write for Room {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.room == 0 {
            return Err(std::io::Error::other("full"));
        }
        let n = buf.len().min(self.room);
        self.room -= n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A standard output that fails, at the first table or at the last line, is an error naming it.
#[test]
fn c10_reports_a_standard_output_that_fails() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-gc-stdout");
    std::fs::write(dir.path().join("listing.tsv"), listing_text().0).unwrap();
    let whole = run_on(&dir, None).unwrap().len();
    for room in [0, whole - 1] {
        let args = Args {
            listing: dir.path().join("listing.tsv"),
            dead_keys: None,
            out: dir.path().join("c10.json"),
        };
        let e = run(&args, &mut Room { room }).unwrap_err();
        assert!(
            matches!(e, Error::Io { what: "write", .. }),
            "room {room}: {e}"
        );
        assert_eq!(e.to_string(), "write standard output: full");
    }
}

/// The verb's flags: `--listing` and `--out` are required, `--dead-keys` is optional.
#[test]
fn the_verbs_flags_are_listing_out_and_optional_dead_keys() {
    #[derive(Debug, clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }
    let a = Cli::try_parse_from(["c10", "--listing", "l", "--out", "o"]).unwrap();
    assert_eq!(a.args.listing, PathBuf::from("l"));
    assert_eq!(a.args.out, PathBuf::from("o"));
    assert_eq!(a.args.dead_keys, None);
    let b =
        Cli::try_parse_from(["c10", "--listing", "l", "--out", "o", "--dead-keys", "d"]).unwrap();
    assert_eq!(b.args.dead_keys, Some(PathBuf::from("d")));
    assert!(Cli::try_parse_from(["c10", "--out", "o"]).is_err());
    assert!(Cli::try_parse_from(["c10", "--listing", "l"]).is_err());
}
