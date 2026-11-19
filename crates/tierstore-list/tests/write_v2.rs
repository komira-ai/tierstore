//! The writer of the L2 tree list, the version 2 lists it does not take: `sidecar_from_v2` refuses,
//! with the exact `V2Fault` (and asks for no pack before it has accepted the list), input that is
//! not a version 2 list (`NotV2`), a major, minor or flag it does not know (`Version`), sections not
//! laid out as the version 2 writer lays them out (`Layout`), entry and digest counts that are not
//! equal and at least 1 (`Counts`) and keys that are not 1 to 1024 bytes of UTF-8, back to back,
//! sorted and each once (`Keys`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use support::{pack_bytes, section, small_tree};
use tierstore_list::{PackCells, PackInfo, TreeListL2, V2Fault, WriteError, sidecar_from_v2};

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Where section `index` of the version 2 list `v2` (0 entries, 1 sha256, 2 key offsets, 3 keys,
/// 4 names, 5 ranges) begins, and its length in bytes, as its header says.
fn v2_section(v2: &[u8], index: usize) -> (usize, usize) {
    let offset = u32::from_le_bytes(v2[16 + 8 * index..20 + 8 * index].try_into().unwrap());
    let count = u32::from_le_bytes(v2[20 + 8 * index..24 + 8 * index].try_into().unwrap());
    let len = match index {
        0 | 1 => count as usize * 32,
        2 => (count as usize + 1) * 4,
        5 => count as usize * 24,
        _ => count as usize,
    };
    (offset as usize, len)
}

/// `v2` with `n` zero bytes inserted before section `index`, the offsets of that section and of
/// those after it moved by `n`.
fn insert_gap(v2: &[u8], index: usize, n: usize) -> Vec<u8> {
    let (at, _) = v2_section(v2, index);
    let mut out = v2[..at].to_vec();
    out.extend(std::iter::repeat_n(0u8, n));
    out.extend_from_slice(&v2[at..]);
    for later in index..6 {
        let place = u32::from_le_bytes(v2[16 + 8 * later..20 + 8 * later].try_into().unwrap());
        put_u32(&mut out, 16 + 8 * later, place + n as u32);
    }
    out
}

/// The sections of a version 2 list written by hand: the counts are those of the bytes.
struct Parts {
    entries: Vec<u8>,
    digests: Vec<u8>,
    offsets: Vec<u32>,
    keys: Vec<u8>,
    names: Vec<u8>,
    ranges: Vec<u8>,
}

impl Default for Parts {
    /// The root record, its digest, and no keys, names or ranges.
    fn default() -> Self {
        Self {
            entries: vec![0; 32],
            digests: vec![0; 32],
            offsets: vec![0],
            keys: Vec::new(),
            names: Vec::new(),
            ranges: Vec::new(),
        }
    }
}

impl Parts {
    /// The version 2 list: its header, then each section at the next multiple of 8, zero between.
    fn build(&self) -> Vec<u8> {
        let offsets: Vec<u8> = self.offsets.iter().flat_map(|n| n.to_le_bytes()).collect();
        let sections: [(&[u8], usize); 6] = [
            (&self.entries, self.entries.len() / 32),
            (&self.digests, self.digests.len() / 32),
            (&offsets, self.offsets.len() - 1),
            (&self.keys, self.keys.len()),
            (&self.names, self.names.len()),
            (&self.ranges, self.ranges.len() / 24),
        ];
        let mut out = vec![0u8; 64];
        out[..8].copy_from_slice(b"TSTREE\0\0");
        put_u16(&mut out, 8, 1);
        for (index, (bytes, count)) in sections.iter().enumerate() {
            out.resize(out.len().div_ceil(8) * 8, 0);
            let place = out.len() as u32;
            put_u32(&mut out, 16 + 8 * index, place);
            put_u32(&mut out, 20 + 8 * index, *count as u32);
            out.extend_from_slice(bytes);
        }
        out
    }

    fn with_keys(keys: &[&str]) -> Self {
        let mut parts = Self::default();
        for key in keys {
            parts.keys.extend_from_slice(key.as_bytes());
            parts.offsets.push(parts.keys.len() as u32);
        }
        parts
    }
}

/// A callback that has the cells of a pack of 100 bytes for every key.
fn any_pack(_: &str) -> Option<PackInfo> {
    Some(PackInfo {
        cells: PackCells::of(&pack_bytes(1, 100)),
        upload_part_size: 0,
        shared: false,
    })
}

/// The error of `sidecar_from_v2(v2, ..)`, and that it asked for no pack.
fn v2_fault(v2: &[u8]) -> V2Fault {
    let mut asked = 0;
    let result = sidecar_from_v2(v2, |key| {
        asked += 1;
        any_pack(key)
    });
    assert_eq!(asked, 0, "no pack is asked for before the list is accepted");
    match result {
        Err(WriteError::V2(fault)) => fault,
        other => panic!("expected a V2Fault, got {other:?}"),
    }
}

#[test]
fn v2_fault_not_v2_empty_input() {
    assert_eq!(v2_fault(&[]), V2Fault::NotV2);
}

#[test]
fn v2_fault_not_v2_shorter_than_the_header() {
    let v2 = small_tree().v2();
    assert_eq!(v2_fault(&v2[..63]), V2Fault::NotV2);
    assert_eq!(v2_fault(&v2[..8]), V2Fault::NotV2);
}

#[test]
fn v2_fault_not_v2_wrong_magic() {
    let mut v2 = small_tree().v2();
    v2[0] = b'X';
    assert_eq!(v2_fault(&v2), V2Fault::NotV2);
    let mut v2 = small_tree().v2();
    v2[7] = 1;
    assert_eq!(v2_fault(&v2), V2Fault::NotV2);
}

#[test]
fn v2_fault_not_v2_an_l2_list_given_as_input() {
    let (_, l2) = small_tree().lists();
    assert_eq!(v2_fault(&l2), V2Fault::NotV2);
}

#[test]
fn v2_fault_not_v2_a_version_1_list() {
    assert_eq!(
        v2_fault(
            br#"{"entries":[]}
{"path":"a"}
{"path":"b"}
{"path":"c"}
{"path":"d"}
"#
        ),
        V2Fault::NotV2
    );
}

#[test]
fn v2_fault_version_major_2() {
    let mut v2 = small_tree().v2();
    put_u16(&mut v2, 8, 2);
    assert_eq!(v2_fault(&v2), V2Fault::Version);
}

#[test]
fn v2_fault_version_major_0() {
    let mut v2 = small_tree().v2();
    put_u16(&mut v2, 8, 0);
    assert_eq!(v2_fault(&v2), V2Fault::Version);
}

#[test]
fn v2_fault_version_minor_1() {
    let mut v2 = small_tree().v2();
    put_u16(&mut v2, 10, 1);
    assert_eq!(v2_fault(&v2), V2Fault::Version);
}

#[test]
fn v2_fault_version_a_flag_set() {
    let mut v2 = small_tree().v2();
    put_u32(&mut v2, 12, 1);
    assert_eq!(v2_fault(&v2), V2Fault::Version);
    let mut v2 = small_tree().v2();
    put_u32(&mut v2, 12, 0x8000_0000);
    assert_eq!(v2_fault(&v2), V2Fault::Version);
}

#[test]
fn v2_fault_layout_section_one_byte_later_than_the_next_multiple_of_8() {
    let v2 = insert_gap(&small_tree().v2(), 4, 1);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_section_eight_bytes_later_than_the_next_multiple_of_8() {
    let v2 = insert_gap(&small_tree().v2(), 1, 8);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_first_section_not_at_64() {
    let v2 = insert_gap(&small_tree().v2(), 0, 8);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_section_overlapping_the_one_before() {
    let mut v2 = small_tree().v2();
    put_u32(&mut v2, 16 + 8, 64);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_a_padding_byte_that_is_not_zero() {
    let tree = small_tree();
    let v2 = tree.v2();
    // Two keys: the key offsets are 12 bytes, so 4 padding bytes lie before the keys.
    let (offsets, len) = v2_section(&v2, 2);
    let (keys, _) = v2_section(&v2, 3);
    assert_eq!(offsets + len + 4, keys);
    for at in offsets + len..keys {
        let mut bad = v2.clone();
        bad[at] = 1;
        assert_eq!(v2_fault(&bad), V2Fault::Layout, "padding byte {at}");
    }
}

#[test]
fn v2_fault_layout_a_section_past_the_end() {
    let v2 = small_tree().v2();
    assert_eq!(v2_fault(&v2[..v2.len() - 1]), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_a_count_that_runs_past_the_end() {
    let mut v2 = small_tree().v2();
    put_u32(&mut v2, 20 + 8 * 4, u32::MAX);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_a_section_that_begins_past_the_end() {
    let mut v2 = small_tree().v2();
    let past = v2.len() as u32 + 8;
    put_u32(&mut v2, 16 + 8 * 5, past);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_trailing_bytes() {
    let mut v2 = small_tree().v2();
    v2.push(0);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
    let mut v2 = small_tree().v2();
    v2.extend_from_slice(&[0; 8]);
    assert_eq!(v2_fault(&v2), V2Fault::Layout);
}

#[test]
fn v2_fault_layout_hand_built_list_is_accepted_when_laid_out_as_the_writer_does() {
    let parts = Parts::with_keys(&["a"]);
    assert!(sidecar_from_v2(&parts.build(), any_pack).is_ok());
}

#[test]
fn v2_fault_counts_no_entries() {
    let parts = Parts {
        entries: vec![],
        digests: vec![],
        ..Parts::default()
    };
    assert_eq!(v2_fault(&parts.build()), V2Fault::Counts);
}

#[test]
fn v2_fault_counts_fewer_digests_than_entries() {
    let parts = Parts {
        digests: vec![],
        ..Parts::default()
    };
    assert_eq!(v2_fault(&parts.build()), V2Fault::Counts);
}

#[test]
fn v2_fault_counts_more_digests_than_entries() {
    let parts = Parts {
        digests: vec![0; 64],
        ..Parts::default()
    };
    assert_eq!(v2_fault(&parts.build()), V2Fault::Counts);
}

#[test]
fn v2_fault_counts_digests_for_no_entries() {
    let parts = Parts {
        entries: vec![],
        ..Parts::default()
    };
    assert_eq!(v2_fault(&parts.build()), V2Fault::Counts);
}

#[test]
fn v2_fault_keys_first_offset_not_zero() {
    let mut parts = Parts::with_keys(&["abc"]);
    parts.offsets[0] = 1;
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_empty_key() {
    let mut parts = Parts::with_keys(&["abc"]);
    parts.offsets = vec![0, 0, 3];
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_empty_last_key() {
    let mut parts = Parts::with_keys(&["abc"]);
    parts.offsets = vec![0, 3, 3];
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_key_of_1025_bytes() {
    let long = "a".repeat(1025);
    let parts = Parts::with_keys(&[&long]);
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_key_of_1024_bytes_is_accepted() {
    let long = "a".repeat(1024);
    let parts = Parts::with_keys(&[&long]);
    let l2 = sidecar_from_v2(&parts.build(), any_pack).unwrap();
    // The PACKS record: key offset 0, key length 1024.
    let (packs, count) = section(&l2, 2);
    assert_eq!(count, 1);
    assert_eq!(l2[packs..packs + 8], [0, 0, 0, 0, 0, 4, 0, 0]);
}

#[test]
fn v2_fault_keys_not_utf8() {
    let mut parts = Parts::with_keys(&["ab"]);
    parts.keys = vec![0xff, 0xfe];
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_unsorted() {
    assert_eq!(
        v2_fault(&Parts::with_keys(&["b", "a"]).build()),
        V2Fault::Keys
    );
    assert_eq!(
        v2_fault(&Parts::with_keys(&["a", "c", "b"]).build()),
        V2Fault::Keys
    );
}

#[test]
fn v2_fault_keys_duplicate() {
    assert_eq!(
        v2_fault(&Parts::with_keys(&["a", "a"]).build()),
        V2Fault::Keys
    );
}

#[test]
fn v2_fault_keys_sorted_distinct_keys_are_accepted() {
    let parts = Parts::with_keys(&["a", "a/b", "b"]);
    let l2 = sidecar_from_v2(&parts.build(), any_pack).unwrap();
    assert_eq!(section(&l2, 2).1, 3);
}

#[test]
fn v2_fault_keys_last_not_at_the_end_of_the_blob() {
    let mut parts = Parts::with_keys(&["a"]);
    parts.keys = b"ab".to_vec();
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_end_offset_past_the_blob() {
    let mut parts = Parts::with_keys(&["ab"]);
    parts.offsets = vec![0, 5];
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

#[test]
fn v2_fault_keys_offsets_that_go_backwards() {
    let mut parts = Parts::with_keys(&["abcd"]);
    parts.offsets = vec![0, 3, 2];
    assert_eq!(v2_fault(&parts.build()), V2Fault::Keys);
}

// ---------------------------------------------------------------------------------------------
// The writer takes the entries as they are
// ---------------------------------------------------------------------------------------------

#[test]
fn sidecar_from_v2_leaves_the_entries_to_the_reader() {
    // A root record of all zeros is a file, not the root directory: the writer copies it, the
    // reader refuses it.
    let parts = Parts::default();
    let v2 = parts.build();
    let l2 = sidecar_from_v2(&v2, any_pack).unwrap();
    assert!(TreeListL2::open(l2).is_err());
}

#[test]
fn sidecar_from_v2_of_an_l2_list_is_refused_and_its_projection_is_not_a_list() {
    let (_, l2) = small_tree().lists();
    assert_eq!(
        sidecar_from_v2(&l2, any_pack),
        Err(WriteError::V2(V2Fault::NotV2))
    );
}
