//! Unit tests of the service: the escapes, the kept bytes and the tags.

use super::*;

/// Catches: a request signed over an escaped query (defect: `unescape` leaves `%2F`
/// alone, so the fake signs the escape again and refuses a request it should take).
#[test]
fn escapes_are_decoded_and_what_is_not_one_stays() {
    assert_eq!(unescape("a%2Fb%41"), "a/bA");
    assert_eq!(unescape("100%"), "100%");
    assert_eq!(unescape("%zz%4"), "%zz%4");
    assert_eq!(unescape(""), "");
}

/// Catches: kept bytes that read back other than written (a zero run merged with
/// its neighbour's length wrong, a range cut at the wrong run).
#[test]
fn kept_bytes_read_back_as_written() {
    let mut bytes = vec![0u8; 3 * RUN + 10];
    bytes[RUN + 5] = 7;
    bytes[3 * RUN + 9] = 9;
    let kept = Kept::of(&bytes);
    assert_eq!(kept.len, bytes.len() as u64);
    assert_eq!(kept.read(0, kept.len), bytes);
    assert_eq!(kept.read(RUN as u64, RUN as u64 + 6), bytes[RUN..RUN + 6]);
    let mut joined = Kept::of(&bytes[..RUN]);
    joined.extend(&Kept::of(&bytes[RUN..]));
    assert_eq!(joined.read(1, joined.len), bytes[1..]);
}

/// Catches: consecutive zero runs kept separately (a 5 GiB object of zeros would
/// keep tens of thousands of entries), or merged with a wrong total.
#[test]
fn consecutive_zero_runs_are_kept_as_one_run() {
    let kept = Kept::of(&vec![0u8; 3 * RUN + 1]);
    assert_eq!(kept.runs.len(), 1);
    assert_eq!(kept.runs[0].as_ref().err(), Some(&(3 * RUN as u64 + 1)));
    assert_eq!(kept.len, 3 * RUN as u64 + 1);
    assert_eq!(kept.read(RUN as u64 - 1, RUN as u64 + 1), [0, 0]);
}

/// Asserts that a time is written in the two forms S3 writes it in: the HTTP date, to
/// the weekday (the documented example, and the epoch, a Thursday), and the ISO form
/// of a listing with its milliseconds.
#[test]
fn times_are_written_as_s3_writes_them() {
    assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    assert_eq!(iso_date(1_255_369_830), "2009-10-12T17:50:30.000Z");
}

/// Asserts that the tag of a part is quoted and depends on the bytes of the part and
/// nothing else: the same for the same bytes, another for other bytes, for zeros of
/// another length, and for the same number of zeros with a one in another place.
#[test]
fn the_tag_of_a_part_depends_on_its_bytes() {
    let tag = part_etag(b"hello ");
    assert_eq!(tag, part_etag(b"hello "));
    assert_ne!(tag, part_etag(b"HELLO "));
    assert!(tag.starts_with('"'), "{tag}");
    assert!(tag.ends_with('"'), "{tag}");
    assert_eq!(tag.trim_matches('"').len(), 16);
    let zeros = |n: usize| part_etag(&vec![0_u8; n]);
    assert_eq!(zeros(3 * RUN), zeros(3 * RUN));
    assert_ne!(zeros(3 * RUN), zeros(3 * RUN + 1));
    let mut shifted = vec![0_u8; 2 * RUN];
    shifted[RUN] = 1;
    let mut other = vec![0_u8; 2 * RUN];
    other[RUN + 1] = 1;
    assert_ne!(part_etag(&shifted), part_etag(&other));
}

/// Asserts that the entity tag is the quoted hex MD5 of the bytes (the tag S3 gives an
/// object of one PUT), and does not depend on the runs the bytes are kept in.
#[test]
fn the_entity_tag_is_the_quoted_md5_of_the_bytes() {
    let whole = Kept::of(b"abc");
    assert_eq!(etag_of(&whole), "\"900150983cd24fb0d6963f7d28e17f72\"");
    let mut joined = Kept::of(b"a");
    joined.extend(&Kept::of(b"bc"));
    assert_eq!(etag_of(&joined), etag_of(&whole));
}
