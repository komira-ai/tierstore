//! The directory `extents-v1`: where the files are, and what the scan at start keeps and removes.

use std::collections::HashSet;
use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::files::{ExtentSidecar, data_of, dir_of, make, meta_new_of, meta_of, remove, scan};

/// A digest-shaped name that differs for each `n`.
fn hex(n: u8) -> String {
    format!("{n:02x}{}", "ab".repeat(31))
}

/// Writes the entry `hex` of an object of `size` bytes whose file is `file_len` bytes, with
/// the sidecar saying `extents`.
fn entry(root: &Path, hex: &str, size: u64, file_len: u64, extents: Vec<[u64; 2]>) {
    make(root, hex).unwrap();
    let file = std::fs::File::create(data_of(root, hex)).unwrap();
    file.set_len(file_len).unwrap();
    let sidecar = ExtentSidecar {
        key: "packs/one".to_owned(),
        size,
        tag: "t".to_owned(),
        modified: 7,
        extents,
    };
    std::fs::write(meta_of(root, hex), serde_json::to_vec(&sidecar).unwrap()).unwrap();
}

/// Asserts the names: the entries are under `extents-v1/<aa>/<bb>/`, the data file is named by
/// the hex and the sidecar by the hex and `.ext`, and what is not there is passed over by
/// `remove`.
#[test]
fn the_files_of_an_entry_are_under_extents_v1_by_the_first_digits_of_its_name() {
    let root = Path::new("/c");
    let name = hex(0x12);
    let dir = Path::new("/c/extents-v1/12/ab");
    assert_eq!(dir_of(root, &name), dir);
    assert_eq!(data_of(root, &name), dir.join(&name));
    assert_eq!(meta_of(root, &name), dir.join(format!("{name}.ext")));
    assert_eq!(
        meta_new_of(root, &name),
        dir.join(format!("{name}.ext.new"))
    );
    remove(Path::new("/nonexistent-cache"), &name);
}

/// Asserts that the scan reads a whole entry back with its ranges, size, and the time of its file,
/// and keeps the files; and that a directory that is not there has no entries.
#[test]
fn the_scan_reads_an_entry_back_with_its_ranges() {
    let scratch = TestDir::under(std::env::temp_dir(), "tsext-scan-ok");
    let root = scratch.path();
    assert_eq!(scan(root, &HashSet::new()).found.len(), 0);
    let name = hex(1);
    entry(root, &name, 100, 100, vec![[10, 20], [50, 10]]);
    let scanned = scan(root, &HashSet::new());
    assert_eq!(scanned.discarded, 0);
    let [found] = &scanned.found[..] else {
        panic!("one entry: {scanned:?}");
    };
    assert_eq!(found.hex, name);
    assert_eq!(found.sidecar.key, "packs/one");
    assert_eq!(found.map.pairs(), [[10, 20], [50, 10]]);
    assert!(found.used > 0);
    assert!(data_of(root, &name).is_file());
}

/// Asserts that the scan removes every file that is not part of a usable entry and counts the
/// data files it removed: a file of another size than the sidecar says, a sidecar that is not
/// JSON, a data file with no sidecar, an entry with no range, an entry whose ranges are damaged,
/// an entry that `obj/` holds whole, a sidecar with no data file, the leftover of a write of a
/// sidecar, and a file named like a digest in a place that is not its own.
#[test]
fn the_scan_removes_what_is_not_a_usable_entry() {
    let scratch = TestDir::under(std::env::temp_dir(), "tsext-scan-bad");
    let root = scratch.path();
    let (short, junk, bare, empty, damaged, whole) =
        (hex(1), hex(2), hex(3), hex(4), hex(5), hex(6));
    entry(root, &short, 100, 90, vec![[0, 10]]);
    entry(root, &junk, 100, 100, vec![[0, 10]]);
    std::fs::write(meta_of(root, &junk), b"not json").unwrap();
    entry(root, &bare, 100, 100, vec![[0, 10]]);
    std::fs::remove_file(meta_of(root, &bare)).unwrap();
    entry(root, &empty, 100, 100, vec![]);
    entry(root, &damaged, 100, 100, vec![[0, 200]]);
    entry(root, &whole, 100, 100, vec![[0, 10]]);
    let orphan = hex(7);
    entry(root, &orphan, 100, 100, vec![[0, 10]]);
    std::fs::remove_file(data_of(root, &orphan)).unwrap();
    let leftover = hex(8);
    make(root, &leftover).unwrap();
    std::fs::write(meta_new_of(root, &leftover), b"{").unwrap();
    let strayed = dir_of(root, &hex(1)).join(hex(9));
    std::fs::write(&strayed, b"x").unwrap();
    let kept = hex(10);
    entry(root, &kept, 100, 100, vec![[0, 10]]);

    let held = HashSet::from([whole.as_str()]);
    let scanned = scan(root, &held);
    let names: Vec<&str> = scanned.found.iter().map(|f| f.hex.as_str()).collect();
    assert_eq!(names, [kept.as_str()]);
    // Six data files, the sidecar with no data file, the leftover and the stray file.
    assert_eq!(scanned.discarded, 9);
    for gone in [
        &short, &junk, &bare, &empty, &damaged, &whole, &orphan, &leftover,
    ] {
        assert!(!data_of(root, gone).exists(), "{gone}");
        assert!(!meta_of(root, gone).exists(), "{gone}");
        assert!(!meta_new_of(root, gone).exists(), "{gone}");
    }
    assert!(!strayed.exists());
    assert!(data_of(root, &kept).is_file());
}
