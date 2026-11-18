use super::*;

/// `SplitMix64`: a tiny seeded generator, so every failure reproduces.
pub struct Rng(pub u64);

impl Rng {
    pub fn u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.u64() % n
    }

    /// A value in `lo..=hi`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }

    pub fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }

    pub fn u8(&mut self) -> u8 {
        self.u64() as u8
    }

    pub fn u16(&mut self) -> u16 {
        self.u64() as u16
    }

    pub fn u32(&mut self) -> u32 {
        self.u64() as u32
    }

    pub fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        out.iter_mut().for_each(|b| *b = self.u8());
        out
    }

    pub fn vec(&mut self, lo: usize, hi: usize) -> Vec<u8> {
        let n = self.range(lo, hi);
        self.bytes(n)
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u8()).collect()
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }

    /// A length in `0..=max` that often is 0, 1 or `max`.
    pub fn len(&mut self, max: usize) -> usize {
        match self.below(5) {
            0 => 0,
            1 => 1,
            2 => max,
            _ => self.range(0, max),
        }
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i as u64 + 1) as usize);
        }
    }
}

pub fn flipped(buf: &[u8], bit: usize) -> Vec<u8> {
    let mut v = buf.to_vec();
    v[bit / 8] ^= 1 << (bit % 8);
    v
}

/// A damaged copy: flipped bits, an overwritten run, a truncation, an extension or an insert.
pub fn mutate(r: &mut Rng, buf: &[u8]) -> Vec<u8> {
    let mut v = buf.to_vec();
    match r.below(6) {
        0 if !v.is_empty() => {
            for _ in 0..r.range(1, 3) {
                let bit = r.below(v.len() as u64 * 8) as usize;
                v[bit / 8] ^= 1 << (bit % 8);
            }
        }
        1 if !v.is_empty() => {
            let at = r.below(v.len() as u64) as usize;
            let n = r.range(1, 8).min(v.len() - at);
            v[at..at + n].copy_from_slice(&r.bytes(n));
        }
        2 if !v.is_empty() => v.truncate(r.below(v.len() as u64) as usize),
        3 => {
            let n = r.range(1, 40);
            v.extend(r.bytes(n));
        }
        4 if !v.is_empty() => {
            let at = r.below(v.len() as u64) as usize;
            v.insert(at, r.u8());
        }
        _ if !v.is_empty() => {
            let at = r.below(v.len() as u64) as usize;
            v.remove(at);
        }
        _ => v.push(r.u8()),
    }
    v
}

/// Rewrites the trailing crc32c of a format whose crc covers every byte before it.
pub fn refix_tail(v: &mut [u8]) {
    if let Some(n) = v.len().checked_sub(4) {
        let crc = crc32c(&v[..n]);
        v[n..].copy_from_slice(&crc.to_le_bytes());
    }
}

/// Overwrites one to three bytes before the crc (at the front, the back or anywhere) and repairs
/// the crc, so the decode runs past it.
pub fn mutate_refixed(r: &mut Rng, buf: &[u8]) -> Vec<u8> {
    let mut v = buf.to_vec();
    let body = v.len().saturating_sub(4);
    if body > 0 {
        for _ in 0..r.range(1, 3) {
            let at = match r.below(3) {
                0 => r.below(body.min(64) as u64) as usize,
                1 => body - 1 - r.below(body.min(96) as u64) as usize,
                _ => r.below(body as u64) as usize,
            };
            v[at] = r.u8();
        }
    }
    refix_tail(&mut v);
    v
}

pub fn gen_key(r: &mut Rng) -> Vec<u8> {
    let n = match r.below(4) {
        0 => r.len(8),
        1 => r.range(40, 300),
        _ => r.range(1, 40),
    };
    r.bytes(n)
}

pub fn gen_header(r: &mut Rng) -> RecordHeader {
    RecordHeader {
        flags: RecordFlags(r.u16()),
        key_len: r.u16(),
        data_len: r.u64(),
        generation: Generation(r.array()),
        stamp: Stamp::from_raw(r.u64()),
        object_offset: r.u64(),
        data_crc: r.u32(),
        key_crc: r.u32(),
    }
}

pub fn gen_cells(r: &mut Rng) -> (u64, Vec<u32>) {
    let special = [
        0,
        1,
        CELL_BYTES - 1,
        CELL_BYTES,
        CELL_BYTES + 1,
        2 * CELL_BYTES + 7,
    ];
    let len = if r.chance(2) {
        *r.pick(&special)
    } else {
        r.below(3 * CELL_BYTES)
    };
    (len, (0..cells_for(len)).map(|_| r.u32()).collect())
}

pub fn gen_trailer(r: &mut Rng) -> Trailer {
    let kind = match r.below(4) {
        0 => BlockKind::Segment,
        1 => BlockKind::Pack,
        2 => BlockKind::ChainSlice,
        _ => BlockKind::Other(*r.pick(&[0, 4, 9, 200, 255])),
    };
    let mut entries: Vec<TrailerEntry> = (0..r.range(0, 8))
        .map(|i| TrailerEntry {
            key: gen_key(r),
            stamp: Stamp::from_raw(r.u64()),
            offset: i as u64 * 4096 + r.below(4096),
            length: r.u64(),
            generation: Generation(r.array()),
            crc32c: r.u32(),
            flags: RecordFlags(r.u16()),
        })
        .collect();
    r.shuffle(&mut entries);
    let (data_len, cell_crcs) = gen_cells(r);
    Trailer {
        kind,
        entries,
        cell_crcs,
        data_len,
        blake3: r.array(),
    }
}

pub fn gen_loc(r: &mut Rng) -> ShardLoc {
    ShardLoc {
        node: NodeId(r.u8()),
        drive: DriveId(r.array()),
    }
}

pub fn gen_desc(r: &mut Rng) -> StripeDescriptor {
    let code = if r.chance(2) {
        let k = if r.chance(10) {
            r.range(50, 200)
        } else {
            r.range(1, 12)
        };
        Code::Ec {
            k: k as u8,
            m: r.range(1, 8) as u8,
        }
    } else {
        Code::Replicas(r.range(1, 6) as u8)
    };
    let k = usize::from(code.data_blocks());
    let j = r.range(1, k.min(10));
    let blocks = (0..j)
        .map(|_| (BlockId::from_raw(r.u64()), r.u64()))
        .collect();
    let mut shards = Vec::new();
    for i in 0..usize::from(code.width()).min(256) {
        let virtual_zero = matches!(code, Code::Ec { .. }) && i >= j && i < k;
        if !virtual_zero && r.chance(2) {
            shards.push(PlacedShard {
                index: i as u8,
                loc: gen_loc(r),
            });
        }
    }
    StripeDescriptor {
        stripe: StripeId::from_raw(r.u64()),
        revision: r.u32(),
        code,
        blocks,
        shards,
        expiry: ExpiryClass(r.u8()),
        coordinator: NodeId(r.u8()),
    }
}

pub fn gen_footer(r: &mut Rng) -> ShardFooter {
    let subject = if r.chance(2) {
        FooterSubject::Stripe {
            stripe: gen_desc(r),
            index: r.u8(),
        }
    } else {
        let mut copies = Vec::new();
        for i in 0..=255u8 {
            if r.chance(8) {
                copies.push(PlacedShard {
                    index: i,
                    loc: gen_loc(r),
                });
            }
        }
        FooterSubject::Copy {
            block: BlockId::from_raw(r.u64()),
            length: r.u64(),
            expiry: ExpiryClass(r.u8()),
            copies,
        }
    };
    let (data_len, cell_crcs) = gen_cells(r);
    ShardFooter {
        subject,
        data_len,
        cell_crcs,
        blake3: r.array(),
    }
}

pub fn gen_seal(r: &mut Rng) -> SealState {
    let ballot = |r: &mut Rng| Ballot {
        round: r.u32(),
        node: NodeId(r.u8()),
    };
    let proposal = |r: &mut Rng| SealProposal {
        length: r.u64(),
        running_crc: r.u32(),
    };
    SealState {
        block: BlockId::from_raw(r.u64()),
        promised: ballot(r),
        accepted: if r.chance(2) {
            Some((ballot(r), proposal(r)))
        } else {
            None
        },
        committed: if r.chance(2) { Some(proposal(r)) } else { None },
    }
}

pub fn gen_free(r: &mut Rng) -> StripeFree {
    StripeFree {
        stripe: StripeId::from_raw(r.u64()),
        revision: r.u32(),
        owner: NodeId(r.u8()),
        stamp: Stamp::from_raw(r.u64()),
    }
}

pub fn unknown_kind(r: &mut Rng, known: &[u8]) -> u8 {
    loop {
        let k = r.u8();
        if !known.contains(&k) {
            return k;
        }
    }
}

pub fn gen_named(r: &mut Rng) -> Named {
    match r.below(3) {
        0 => Named::Block(BlockId::from_raw(r.u64())),
        1 => Named::LargeFile(LargeFile {
            key_hash: KeyHash(r.array()),
            generation: Generation(r.array()),
            size: r.u64(),
        }),
        _ => {
            let n = r.len(30);
            Named::Unknown {
                kind: unknown_kind(r, &[1, 2]),
                bytes: r.bytes(n),
            }
        }
    }
}

pub fn gen_log_record(r: &mut Rng, origin: NodeId, seq: u64) -> LogRecord {
    let entry = match r.below(4) {
        0 => LogEntry::Tombstone(Tombstone {
            key: gen_key(r),
            generation: Generation(r.array()),
            stamp: Stamp::from_raw(r.u64()),
            names: (0..r.range(0, 5)).map(|_| gen_named(r)).collect(),
        }),
        1 => LogEntry::BlockDrop(BlockDrop {
            block: BlockId::from_raw(r.u64()),
            stamp: Stamp::from_raw(r.u64()),
        }),
        2 => LogEntry::TombDrop(TombDrop {
            target_origin: NodeId(r.u8()),
            target_seq: r.u64(),
        }),
        _ => {
            let n = r.len(40);
            LogEntry::Unknown {
                kind: unknown_kind(r, &[1, 2, 3]),
                body: r.bytes(n),
            }
        }
    };
    LogRecord { origin, seq, entry }
}

pub fn gen_any_log_record(r: &mut Rng) -> LogRecord {
    let (origin, seq) = (NodeId(r.u8()), r.u64().max(1));
    gen_log_record(r, origin, seq)
}

pub fn gen_hot(r: &mut Rng) -> HotName {
    let block = BlockId::from_raw(r.u64());
    let stripe = StripeId::from_raw(r.u64());
    match r.below(10) {
        0 => HotName::Segment(block),
        1 => HotName::SegmentIndex(block),
        2 => HotName::Large(LargeFile {
            key_hash: KeyHash(r.array()),
            generation: Generation(r.array()),
            size: r.u64(),
        }),
        3 => HotName::LargeUpload {
            key_hash: KeyHash(r.array()),
            nonce: r.u64(),
        },
        4 => HotName::Cache {
            boot: r.u64(),
            seq: r.u64(),
        },
        5 => HotName::TombLog {
            origin: NodeId(r.u8()),
            first_seq: r.u64(),
        },
        6 => HotName::BlockIndex(block),
        7 => HotName::Descriptor(stripe),
        8 => HotName::Free(stripe),
        _ => HotName::Seal(block),
    }
}

pub fn gen_cold(r: &mut Rng) -> ColdName {
    let stripe = StripeId::from_raw(r.u64());
    let block = BlockId::from_raw(r.u64());
    match r.below(4) {
        0 => ColdName::Shard {
            stripe,
            index: r.u8(),
        },
        1 => ColdName::Copy(block),
        2 => ColdName::ShardUpload {
            stripe,
            index: r.u8(),
            nonce: r.u64(),
        },
        _ => ColdName::CopyUpload {
            block,
            nonce: r.u64(),
        },
    }
}

pub fn invalid(what: &'static str, reason: &'static str) -> FormatError {
    FormatError::Invalid { what, reason }
}
