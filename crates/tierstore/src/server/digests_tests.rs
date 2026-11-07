//! Tests of the table of hashes and of the accumulator that computes them in one pass.

use super::digests::{Algorithm, Digests, TRAILABLE};

/// The check string of every catalogue of hashes: nine ASCII digits.
const CHECK: &[u8] = b"123456789";

/// The hash of `body` for `algorithm`, in the accumulator, fed in pieces of `piece` bytes.
fn computed(algorithm: Algorithm, body: &[u8], piece: usize) -> Vec<u8> {
    let mut digests = Digests::new([algorithm]);
    for chunk in body.chunks(piece) {
        digests.update(chunk);
    }
    digests.finish().remove(&algorithm).unwrap()
}

/// A body of `len` bytes that no hash sees as a pattern.
fn body_of(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (state >> 56) as u8
    };
    (0..len).map(|_| next()).collect()
}

/// Asserts that each algorithm gives, for the check string `123456789`, the value its standard
/// publishes: CRC-32 `CBF43926`, CRC-32C `E3069283` and CRC-64/NVMe `AE8B14860A799888` (the
/// check values of their catalogues), and the MD5, SHA-1 and SHA-256 of the string. It catches a
/// polynomial, an initial value, a final xor or a byte order that is not S3's.
#[test]
fn known_answers_for_the_check_string() {
    let known = [
        (Algorithm::Md5, "25f9e794323b453885f5181f1b624d0b"),
        (Algorithm::Crc32, "cbf43926"),
        (Algorithm::Crc32c, "e3069283"),
        (Algorithm::Sha1, "f7c3bc1d808e04732adf679965ccc34ca7ae3441"),
        (
            Algorithm::Sha256,
            "15e2b0d3c33891ebb0f1ef609ec419420c20e320ce94c65fbc8c3312448eb225",
        ),
        (Algorithm::Crc64Nvme, "ae8b14860a799888"),
    ];
    assert_eq!(known.map(|(algorithm, _)| algorithm), Algorithm::ALL);
    for (algorithm, hex) in known {
        let got = computed(algorithm, CHECK, CHECK.len());
        assert_eq!(hex::encode(got), hex, "{}", algorithm.header());
    }
}

/// Asserts that the hash of a body does not depend on where the pieces it came in were cut:
/// whole, in pieces of one byte, of a prime size, and with empty pieces between, for every
/// algorithm, on a body longer than the 16 slices of the CRC-64 table.
#[test]
fn digest_is_independent_of_chunk_boundaries() {
    let body = body_of(1000);
    for algorithm in Algorithm::ALL {
        let whole = computed(algorithm, &body, body.len());
        assert_eq!(whole.len(), algorithm.length(), "{}", algorithm.header());
        for piece in [1, 7, 16, 17, 333] {
            let got = computed(algorithm, &body, piece);
            assert_eq!(got, whole, "{} in pieces of {piece}", algorithm.header());
        }
        let mut digests = Digests::new([algorithm]);
        for chunk in body.chunks(100) {
            digests.update(&[]);
            digests.update(chunk);
        }
        assert_eq!(digests.finish().remove(&algorithm).unwrap(), whole);
    }
}

/// Asserts that the hash of nothing is what each standard gives for an empty body, so a body
/// with no piece at all is held to the right value: MD5 `d41d8cd9...`, CRC-32 and CRC-32C zero,
/// CRC-64/NVMe zero.
#[test]
fn an_empty_body_has_the_hash_of_nothing() {
    let digests = Digests::new(Algorithm::ALL).finish();
    let hex = |algorithm: Algorithm| hex::encode(&digests[&algorithm]);
    assert_eq!(hex(Algorithm::Md5), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(hex(Algorithm::Crc32), "00000000");
    assert_eq!(hex(Algorithm::Crc32c), "00000000");
    assert_eq!(hex(Algorithm::Crc64Nvme), "0000000000000000");
    assert_eq!(
        hex(Algorithm::Sha1),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
}

/// Asserts that an algorithm named more than once is computed once, and that all algorithms of
/// one accumulator see the same pieces: the accumulator of a list with repeats has one hash for
/// each distinct algorithm, equal to the hash of an accumulator of that algorithm alone.
#[test]
fn an_algorithm_named_twice_is_hashed_once() {
    let twice = [
        Algorithm::Sha256,
        Algorithm::Crc64Nvme,
        Algorithm::Sha256,
        Algorithm::Crc64Nvme,
    ];
    let mut digests = Digests::new(twice);
    digests.update(CHECK);
    let done = digests.finish();
    assert_eq!(done.len(), 2);
    for algorithm in [Algorithm::Sha256, Algorithm::Crc64Nvme] {
        assert_eq!(done[&algorithm], computed(algorithm, CHECK, 4));
    }
}

/// Asserts that the table is consistent: every algorithm is found again by its header, a header
/// that is not a hash of a body is no algorithm, every hash has the length the table says, and
/// the algorithms a trailer may carry are all but MD5 (S3 takes `Content-MD5` only as a header).
#[test]
fn the_table_of_algorithms_is_consistent() {
    for algorithm in Algorithm::ALL {
        let header = algorithm.header();
        assert_eq!(Algorithm::from_header(header), Some(algorithm), "{header}");
        assert_eq!(
            algorithm.trailable(),
            algorithm != Algorithm::Md5,
            "{header}"
        );
    }
    for header in ["x-amz-checksum-md5", "content-length", "", "Content-MD5"] {
        assert_eq!(Algorithm::from_header(header), None, "{header:?}");
    }
    assert_eq!(TRAILABLE.len(), Algorithm::ALL.len() - 1);
    assert!(TRAILABLE.iter().all(|algorithm| algorithm.trailable()));
}
