//! The text of the cache's block of `/metricz`: one `# TYPE` line for each family, before its
//! samples, with the kind the metrics page gives it.

use std::collections::BTreeMap;

use super::metrics::{Counters, LABELLED, PLAIN, queued_reads, short_objects};
use crate::ShortObjects;

/// The families of `text` in the order they come, each with its kind and its samples, or a
/// problem: a sample with no `# TYPE` line for its family before it, a family typed twice, or
/// samples of one family that are not together.
fn families(text: &str) -> Result<Vec<(String, String, Vec<String>)>, String> {
    let mut found: Vec<(String, String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(typed) = line.strip_prefix("# TYPE ") {
            let (name, kind) = typed.split_once(' ').ok_or(format!("bad line {line:?}"))?;
            if found.iter().any(|(seen, _, _)| seen == name) {
                return Err(format!("{name} is typed twice"));
            }
            found.push((name.to_owned(), kind.to_owned(), Vec::new()));
            continue;
        }
        let name = line.split(['{', ' ']).next().unwrap_or_default();
        match found.last_mut() {
            Some((typed, _, samples)) if typed == name => samples.push(line.to_owned()),
            _ => {
                return Err(format!(
                    "{line:?} has no # TYPE line just before its family"
                ));
            }
        }
    }
    Ok(found)
}

/// Asserts that every family of the block has exactly one `# TYPE` line, before its first
/// sample, that its samples follow it together, that the gauges (bytes, objects, free
/// bytes, staging files, the mode, the depth of the prefetch queue, the holds open, the requests of the fill socket that wait or run and the reads of the upstream in flight) are typed `gauge` and every other family `counter`, and
/// that a labelled family has one sample for each of its values. It fails on a sample with no
/// type, a family typed twice or with the wrong kind, and a missing label value.
#[test]
fn every_family_has_one_type_line_of_its_kind_before_its_samples() {
    let text = Counters::default().render(3, 2, 1, true, 4, 5);
    let found = families(&text).unwrap_or_else(|problem| panic!("{problem}\n{text}"));
    let kinds: BTreeMap<&str, &str> = found
        .iter()
        .map(|(name, kind, _)| (name.as_str(), kind.as_str()))
        .collect();
    let gauges = [
        "tierstore_cache_bytes",
        "tierstore_cache_objects",
        "tierstore_cache_fs_free_bytes",
        "tierstore_cache_staging_files",
        "tierstore_cache_mode",
        "tierstore_cache_prefetch_queue_depth",
        "tierstore_cache_holds_open",
        "tierstore_cache_fill_socket_waiting",
        "tierstore_cache_fill_socket_running",
        "tierstore_cache_upstream_reads_in_flight",
    ];
    for (name, kind) in &kinds {
        let want = if gauges.contains(name) {
            "gauge"
        } else {
            "counter"
        };
        assert_eq!(*kind, want, "{name}");
    }
    assert_eq!(found.len(), PLAIN.len() + LABELLED.len(), "{text}");
    for (name, _) in PLAIN {
        let (_, _, samples) = found.iter().find(|(n, _, _)| n == name).expect(name);
        assert_eq!(samples.len(), 1, "{name}");
    }
    for (name, _, label, values) in LABELLED {
        let (_, _, samples) = found.iter().find(|(n, _, _)| n == name).expect(name);
        let want: Vec<String> = values
            .iter()
            .map(|value| format!("{name}{{{label}=\"{value}\"}}"))
            .collect();
        let got: Vec<&str> = samples
            .iter()
            .map(|sample| sample.rsplit_once(' ').map_or("", |(series, _)| series))
            .collect();
        assert_eq!(got, want, "{name}");
    }
    assert!(text.contains("tierstore_cache_bytes 3\n"), "{text}");
    assert!(text.contains("tierstore_cache_mode{mode=\"read_write\"} 1\n"));
    assert!(text.contains("tierstore_cache_mode{mode=\"read_only\"} 0\n"));
}

/// Asserts that the reader of the text above finds each defect it is there to find: a sample
/// with no type line, a type line with no kind, a family typed twice, and samples of a family
/// that are apart.
#[test]
fn the_reader_of_the_text_finds_each_defect() {
    let untyped = families("tierstore_cache_hits_total 0\n");
    assert!(untyped.unwrap_err().contains("no # TYPE line"));
    let kindless = families("# TYPE tierstore_cache_hits_total\n");
    assert_eq!(
        kindless.unwrap_err(),
        "bad line \"# TYPE tierstore_cache_hits_total\""
    );
    let twice = "# TYPE a counter\na 1\n# TYPE a counter\na 2\n";
    assert_eq!(families(twice).unwrap_err(), "a is typed twice");
    let apart = "# TYPE a counter\na{x=\"1\"} 1\n# TYPE b gauge\nb 2\na{x=\"2\"} 3\n";
    assert!(families(apart).unwrap_err().contains("no # TYPE line"));
    let good = families("# TYPE a counter\na 1\n").unwrap();
    assert_eq!(good, [("a".into(), "counter".into(), vec!["a 1".into()])]);
}

/// Asserts the text of the two families of the reads the mount provider queues, in full: the
/// bytes held, a gauge, and the preparations refused room, a counter, each with its type line.
#[test]
fn the_queued_reads_are_a_gauge_of_the_bytes_and_a_counter_of_the_refusals() {
    assert_eq!(
        queued_reads(4096, 1),
        "# TYPE tierstore_provider_prefetch_queued_bytes gauge\n\
         tierstore_provider_prefetch_queued_bytes 4096\n\
         # TYPE tierstore_provider_prefetch_skipped_total counter\n\
         tierstore_provider_prefetch_skipped_total 1\n"
    );
}

/// Asserts the text of the families of what the S3 upstream did after its failed writes, in full:
/// the failed aborts kept, a gauge, those dropped, a counter, and the looks after puts that got no
/// answer by result, `skipped` among them, each value in its own series.
#[test]
fn the_failed_aborts_are_a_gauge_and_the_looks_count_their_skips() {
    let short = ShortObjects {
        removed: 1,
        left: 2,
        failed: 3,
        skipped: 4,
        failed_aborts: 5,
        aborts_dropped: 6,
    };
    assert_eq!(
        short_objects(short),
        "# TYPE tierstore_cache_upstream_failed_aborts gauge\n\
         tierstore_cache_upstream_failed_aborts 5\n\
         # TYPE tierstore_cache_upstream_failed_aborts_dropped_total counter\n\
         tierstore_cache_upstream_failed_aborts_dropped_total 6\n\
         # TYPE tierstore_cache_upstream_short_objects_total counter\n\
         tierstore_cache_upstream_short_objects_total{result=\"removed\"} 1\n\
         tierstore_cache_upstream_short_objects_total{result=\"left\"} 2\n\
         tierstore_cache_upstream_short_objects_total{result=\"failed\"} 3\n\
         tierstore_cache_upstream_short_objects_total{result=\"skipped\"} 4\n"
    );
    assert!(families(&short_objects(short)).is_ok());
}
