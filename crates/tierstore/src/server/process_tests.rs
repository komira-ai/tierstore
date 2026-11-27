//! The process's figures: their text on `/metricz`, and on Linux how the fields of procfs make
//! them, read from a procfs a test writes, never the machine's.

use super::*;

/// Asserts that every figure renders as its `# TYPE` line and its sample, in full and by exact
/// equality, whole numbers as integers and seconds with their fraction.
#[test]
fn every_figure_renders_its_type_and_its_sample() {
    let figures = Figures {
        resident_bytes: Some(1_048_576),
        virtual_bytes: Some(4_194_304),
        start_seconds: Some(1_790_000_000.25),
        cpu_seconds: Some(1.5),
        open_fds: Some(12),
    };
    assert_eq!(
        figures.render(),
        "# TYPE process_resident_memory_bytes gauge\n\
         process_resident_memory_bytes 1048576\n\
         # TYPE process_virtual_memory_bytes gauge\n\
         process_virtual_memory_bytes 4194304\n\
         # TYPE process_start_time_seconds gauge\n\
         process_start_time_seconds 1790000000.25\n\
         # TYPE process_cpu_seconds_total counter\n\
         process_cpu_seconds_total 1.5\n\
         # TYPE process_open_fds gauge\n\
         process_open_fds 12\n"
    );
}

/// Asserts that a figure that was not read has no line at all, not a zero, and that a source of
/// none reads none.
#[test]
fn a_figure_not_read_has_no_line() {
    let some = Figures {
        cpu_seconds: Some(0.0),
        open_fds: Some(3),
        ..Figures::default()
    };
    assert_eq!(
        some.render(),
        "# TYPE process_cpu_seconds_total counter\n\
         process_cpu_seconds_total 0\n\
         # TYPE process_open_fds gauge\n\
         process_open_fds 3\n"
    );
    assert_eq!(Source::None.read(), Figures::default());
    assert_eq!(Figures::default().render(), "");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    use tierstore_testkit::dir::TestDir;

    use super::super::*;

    /// A line of `self/stat` whose name has spaces and parentheses, with `utime` 250, `stime`
    /// 150, `starttime` 4200, `vsize` 8192000 and `rss` 300.
    const STAT: &str = "4242 (tier) (store x) S 1 4242 4242 0 -1 4194560 100 0 0 0 250 150 0 0 \
                        20 0 9 0 4200 8192000 300 18446744073709551615\n";

    /// A `stat` of procfs with its boot time.
    const PROC_STAT: &str = "cpu  1 2 3 4\nintr 5\nbtime 1790000000\nprocesses 77\n";

    /// Asserts that the fields are counted from the last `)`, so that a name with spaces and
    /// parentheses does not move them, and that each field is the one its number names.
    #[test]
    fn the_fields_of_stat_are_counted_from_the_end_of_the_name() {
        assert_eq!(
            parse_stat(STAT),
            Stat {
                user_ticks: Some(250),
                system_ticks: Some(150),
                start_ticks: Some(4200),
                virtual_bytes: Some(8_192_000),
                resident_pages: Some(300),
            }
        );
    }

    /// Asserts that a text with no name has no field, that a short one has the fields it holds
    /// and none past them, and that a field that is not a whole number (`rss` negative) is
    /// missing alone.
    #[test]
    fn a_stat_that_is_short_or_not_numbers_has_those_fields_missing() {
        assert_eq!(parse_stat("4242 tierstore S 1"), Stat::default());
        assert_eq!(parse_stat(""), Stat::default());
        let short = "1 (t) S 1 1 1 0 -1 0 0 0 0 0 7 8 0 0";
        assert_eq!(
            parse_stat(short),
            Stat {
                user_ticks: Some(7),
                system_ticks: Some(8),
                ..Stat::default()
            }
        );
        let negative = "1 (t) S 1 1 1 0 -1 0 0 0 0 0 7 8 0 0 20 0 1 0 9 4096 -1";
        assert_eq!(
            parse_stat(negative),
            Stat {
                user_ticks: Some(7),
                system_ticks: Some(8),
                start_ticks: Some(9),
                virtual_bytes: Some(4096),
                resident_pages: None,
            }
        );
    }

    /// Asserts that the boot time is the `btime` line's, and is missing from a text without one
    /// or with one that is not a number.
    #[test]
    fn the_boot_time_is_the_btime_line() {
        assert_eq!(parse_boot_time(PROC_STAT), Some(1_790_000_000));
        assert_eq!(parse_boot_time("cpu 1 2\nprocesses 3\n"), None);
        assert_eq!(parse_boot_time("btime soon\n"), None);
    }

    /// Asserts how the fields make the figures: the CPU time is user and system over the ticks
    /// of a second, the start is the boot time and the start's ticks, the resident memory is the
    /// pages times the page.
    #[test]
    fn the_figures_are_the_fields_in_seconds_and_bytes() {
        let stat = parse_stat(STAT);
        assert_eq!(
            figures_of(&stat, Some(1_790_000_000), Some(9), 100, 4096),
            Figures {
                resident_bytes: Some(300 * 4096),
                virtual_bytes: Some(8_192_000),
                start_seconds: Some(1_790_000_042.0),
                cpu_seconds: Some(4.0),
                open_fds: Some(9),
            }
        );
    }

    /// Asserts that a figure that would be wrong is left out: the seconds with no ticks in a
    /// second, the start with no boot time or a boot at zero, a sum or a product that does not
    /// fit, and a memory of zero, which a running process cannot have.
    #[test]
    fn a_figure_that_would_be_wrong_is_left_out() {
        let stat = parse_stat(STAT);
        let no_ticks = figures_of(&stat, Some(1_790_000_000), None, 0, 4096);
        assert_eq!((no_ticks.cpu_seconds, no_ticks.start_seconds), (None, None));
        assert_eq!(no_ticks.resident_bytes, Some(300 * 4096));
        assert_eq!(no_ticks.open_fds, None);
        assert_eq!(figures_of(&stat, None, None, 100, 4096).start_seconds, None);
        assert_eq!(figures_of(&stat, Some(0), None, 100, 4096).start_seconds, None);
        let huge = Stat {
            user_ticks: Some(u64::MAX),
            system_ticks: Some(1),
            start_ticks: Some(1),
            virtual_bytes: Some(0),
            resident_pages: Some(u64::MAX),
        };
        let figures = figures_of(&huge, Some(1), None, 100, 4096);
        assert_eq!(
            (figures.cpu_seconds, figures.resident_bytes, figures.virtual_bytes),
            (None, None, None)
        );
        let zero = Stat {
            resident_pages: Some(0),
            ..Stat::default()
        };
        assert_eq!(figures_of(&zero, None, None, 100, 4096), Figures::default());
    }

    /// Writes a procfs under `root`: `stat`, `self/stat` and `fds` entries in `self/fd`.
    fn write_procfs(root: &Path, stat: &str, proc_stat: &str, fds: usize) {
        std::fs::create_dir_all(root.join("self/fd")).unwrap();
        std::fs::write(root.join("self/stat"), stat).unwrap();
        std::fs::write(root.join("stat"), proc_stat).unwrap();
        for fd in 0..fds {
            std::fs::write(root.join(format!("self/fd/{fd}")), "").unwrap();
        }
    }

    /// Asserts that a source over a procfs reads its `stat`, its `self/stat` and the entries of
    /// its `self/fd`, in this system's ticks and pages.
    #[test]
    fn a_procfs_source_reads_its_files() {
        let dir = TestDir::under(std::env::temp_dir(), "tsprocess-read");
        write_procfs(dir.path(), STAT, PROC_STAT, 5);
        let source = Source::Procfs {
            root: dir.path().to_path_buf(),
            ticks: 100,
            page: 4096,
            start: std::sync::OnceLock::new(),
        };
        assert_eq!(
            source.read(),
            Figures {
                resident_bytes: Some(300 * 4096),
                virtual_bytes: Some(8_192_000),
                start_seconds: Some(1_790_000_042.0),
                cpu_seconds: Some(4.0),
                open_fds: Some(5),
            }
        );
        let Source::Procfs { ticks, page, .. } = Source::procfs(dir.path()) else {
            panic!("a procfs source");
        };
        assert!(ticks > 0 && page > 0, "{ticks} {page}");
    }

    /// Asserts that a procfs with none of the files gives no figure, and so no line.
    #[test]
    fn a_procfs_with_no_files_gives_no_figure() {
        let dir = TestDir::under(std::env::temp_dir(), "tsprocess-empty");
        let source = Source::Procfs {
            root: dir.path().to_path_buf(),
            ticks: 100,
            page: 4096,
            start: std::sync::OnceLock::new(),
        };
        assert_eq!(source.read(), Figures::default());
        assert_eq!(source.read().render(), "");
    }

    /// Asserts that the start is the first one read and stays so: before `stat` exists there is
    /// none; once it does, its start is shown; and when the boot time moves, as a step of the wall
    /// clock moves it, the start shown does not.
    #[test]
    fn the_start_read_first_stays_when_the_boot_time_moves() {
        let dir = TestDir::under(std::env::temp_dir(), "tsprocess-start");
        std::fs::create_dir_all(dir.path().join("self/fd")).unwrap();
        std::fs::write(dir.path().join("self/stat"), STAT).unwrap();
        let source = Source::Procfs {
            root: dir.path().to_path_buf(),
            ticks: 100,
            page: 4096,
            start: std::sync::OnceLock::new(),
        };
        assert_eq!(source.read().start_seconds, None);
        std::fs::write(dir.path().join("stat"), PROC_STAT).unwrap();
        assert_eq!(source.read().start_seconds, Some(1_790_000_042.0));
        std::fs::write(dir.path().join("stat"), "btime 1790000005\n").unwrap();
        assert_eq!(source.read().start_seconds, Some(1_790_000_042.0));
    }
}
