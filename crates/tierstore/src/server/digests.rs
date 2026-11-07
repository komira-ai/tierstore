//! The hashes a body can be held to, and the one pass that computes them.
//!
//! [`Algorithm`] is the table of the hashes S3 knows for a body: `Content-MD5` and the five
//! `x-amz-checksum-*` headers, each with the length in bytes of its value. [`Digests`] is the
//! accumulator: a set of algorithms, each hashed once however many claims name it, fed every
//! piece of a body as it passes, and read at the end. A hash is its bytes (the digest, or the
//! big-endian bytes of a CRC), which is also the form the headers carry in base64.

use std::collections::HashMap;

use crc::{CRC_64_NVME, Crc, Digest as Crc64Digest, Table};
use crc32fast::Hasher as Crc32;
use md5::Md5;
use ring::digest::{Context as Sha1Context, SHA1_FOR_LEGACY_USE_ONLY};
use sha2::{Digest, Sha256};

/// CRC-64/NVMe, with the table of 16 slices that makes it fast enough for a body at line rate.
static CRC64_NVME: Crc<u64, Table<16>> = Crc::<u64, Table<16>>::new(&CRC_64_NVME);

/// A hash a request may claim for its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Algorithm {
    Md5,
    Crc32,
    Crc32c,
    Sha1,
    Sha256,
    Crc64Nvme,
}

/// The algorithms S3 takes in the trailer of an `aws-chunked` body: every one but MD5.
pub(crate) const TRAILABLE: [Algorithm; 5] = [
    Algorithm::Crc32,
    Algorithm::Crc32c,
    Algorithm::Sha1,
    Algorithm::Sha256,
    Algorithm::Crc64Nvme,
];

impl Algorithm {
    /// Every algorithm, in the order a request's headers are read in.
    pub(crate) const ALL: [Self; 6] = [
        Self::Md5,
        Self::Crc32,
        Self::Crc32c,
        Self::Sha1,
        Self::Sha256,
        Self::Crc64Nvme,
    ];

    /// The header in which a request claims this hash of its body.
    pub(crate) fn header(self) -> &'static str {
        match self {
            Self::Md5 => "content-md5",
            Self::Crc32 => "x-amz-checksum-crc32",
            Self::Crc32c => "x-amz-checksum-crc32c",
            Self::Sha1 => "x-amz-checksum-sha1",
            Self::Sha256 => "x-amz-checksum-sha256",
            Self::Crc64Nvme => "x-amz-checksum-crc64nvme",
        }
    }

    /// The algorithm whose claim is the header `header` (lower case), if there is one.
    pub(crate) fn from_header(header: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.header() == header)
    }

    /// The length in bytes of a hash of this algorithm.
    pub(crate) fn length(self) -> usize {
        match self {
            Self::Md5 => 16,
            Self::Crc32 | Self::Crc32c => 4,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Crc64Nvme => 8,
        }
    }

    /// Whether S3 takes this hash as the trailing checksum of an `aws-chunked` body.
    pub(crate) fn trailable(self) -> bool {
        TRAILABLE.contains(&self)
    }
}

/// The running state of one algorithm.
enum Hasher {
    Sha256(Sha256),
    Md5(Md5),
    Crc32(Crc32),
    Crc32c(u32),
    Sha1(Sha1Context),
    Crc64Nvme(Crc64Digest<'static, u64, Table<16>>),
}

impl Hasher {
    fn new(algorithm: Algorithm) -> Self {
        match algorithm {
            Algorithm::Md5 => Self::Md5(Md5::new()),
            Algorithm::Crc32 => Self::Crc32(Crc32::new()),
            Algorithm::Crc32c => Self::Crc32c(0),
            Algorithm::Sha1 => Self::Sha1(Sha1Context::new(&SHA1_FOR_LEGACY_USE_ONLY)),
            Algorithm::Sha256 => Self::Sha256(Sha256::new()),
            Algorithm::Crc64Nvme => Self::Crc64Nvme(CRC64_NVME.digest()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(hasher) => Digest::update(hasher, data),
            Self::Md5(hasher) => Digest::update(hasher, data),
            Self::Crc32(hasher) => hasher.update(data),
            Self::Crc32c(state) => *state = crc32c::crc32c_append(*state, data),
            Self::Sha1(context) => context.update(data),
            Self::Crc64Nvme(digest) => digest.update(data),
        }
    }

    /// The hash, as bytes: the digest, or the big-endian bytes of a CRC.
    fn finish(self) -> Vec<u8> {
        match self {
            Self::Sha256(hasher) => hasher.finalize().to_vec(),
            Self::Md5(hasher) => hasher.finalize().to_vec(),
            Self::Crc32(hasher) => hasher.finalize().to_be_bytes().to_vec(),
            Self::Crc32c(state) => state.to_be_bytes().to_vec(),
            Self::Sha1(context) => context.finish().as_ref().to_vec(),
            Self::Crc64Nvme(digest) => digest.finalize().to_be_bytes().to_vec(),
        }
    }
}

/// The hashes of one body, each algorithm computed once, in the one pass the body is read in.
pub(crate) struct Digests {
    hashers: Vec<(Algorithm, Hasher)>,
}

impl Digests {
    /// An accumulator for `algorithms`; an algorithm named twice is hashed once.
    pub(crate) fn new(algorithms: impl IntoIterator<Item = Algorithm>) -> Self {
        let mut hashers: Vec<(Algorithm, Hasher)> = Vec::new();
        for algorithm in algorithms {
            if hashers.iter().all(|(known, _)| *known != algorithm) {
                hashers.push((algorithm, Hasher::new(algorithm)));
            }
        }
        Self { hashers }
    }

    /// Feeds the next piece of the body to every hasher.
    pub(crate) fn update(&mut self, data: &[u8]) {
        for (_, hasher) in &mut self.hashers {
            hasher.update(data);
        }
    }

    /// The hash of the whole body for each algorithm of the accumulator.
    pub(crate) fn finish(self) -> HashMap<Algorithm, Vec<u8>> {
        let done = |(algorithm, hasher): (Algorithm, Hasher)| (algorithm, hasher.finish());
        self.hashers.into_iter().map(done).collect()
    }
}
