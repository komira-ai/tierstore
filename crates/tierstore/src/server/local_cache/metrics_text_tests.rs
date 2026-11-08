//! The text of the cache's block of `/metricz`: one `# TYPE` line for each family, before its
//! samples, with the kind the metrics page gives it.

use std::collections::BTreeMap;

use super::metrics::{Counters, LABELLED, PLAIN};

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
/// sample, that its samples follow it together, that the five gauges (bytes, objects, free
/// bytes, staging files and the mode) are typed `gauge` and every other family `counter`, and
/// that a labelled family has one sample for each of its values. It fails on a sample with no
/// type, a family typed twice or with the wrong kind, and a missing label value.
#[test]
fn every_family_has_one_type_line_of_its_kind_before_its_samples() {
    let text = Counters::default().render(3, 2, 1, true);
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
