//! File names, identity files and id texts.

use super::*;

// Names, identity, ids

const TOKENS: &[&str] = &[
    "seg",
    "big",
    "cache",
    "tomb",
    "blk",
    "stripes",
    "seals",
    "s",
    "/",
    "/",
    ".",
    ".",
    "idx",
    "up",
    "obj",
    "log",
    "desc",
    "free",
    "seal",
    "shard",
    "copy",
    "0",
    "1",
    "00",
    "ff",
    "a",
    "-",
    " ",
    "\u{e9}",
    "0000000000000001",
    "00000000000000000000000000000001",
    "ABCDEF",
    "256",
    "007",
];

const EDIT_CHARS: &[char] = &[
    '0', '1', '7', '9', 'a', 'f', 'A', 'F', '/', '.', ' ', '_', 'x', '\n', '\r', '=', '\u{e9}',
];

fn mutate_str(r: &mut Rng, s: &str) -> String {
    let mut c: Vec<char> = s.chars().collect();
    for _ in 0..r.range(1, 3) {
        let at = if c.is_empty() {
            0
        } else {
            r.below(c.len() as u64) as usize
        };
        match r.below(5) {
            0 if !c.is_empty() => {
                c.remove(at);
            }
            1 => c.insert(at, *r.pick(EDIT_CHARS)),
            2 if !c.is_empty() => c[at] = *r.pick(EDIT_CHARS),
            3 if c.len() > 1 => c.truncate(at),
            4 if c.len() > 1 => {
                let next = (at + 1) % c.len();
                c.swap(at, next);
            }
            _ => c.push(*r.pick(EDIT_CHARS)),
        }
    }
    c.into_iter().collect()
}

#[test]
fn hot_and_cold_names_round_trip_through_their_paths() {
    let mut r = Rng(23);
    for case in 0..3000 {
        let hot = gen_hot(&mut r);
        let path = hot.path();
        assert_eq!(HotName::parse(&path), Some(hot), "case {case} {path}");
        let cold = gen_cold(&mut r);
        let path = cold.path();
        assert_eq!(ColdName::parse(&path), Some(cold), "case {case} {path}");
    }
}

#[test]
fn a_name_that_parses_has_exactly_one_spelling_whatever_the_damage_or_the_text() {
    let mut r = Rng(24);
    let (mut hot_ok, mut cold_ok) = (0, 0);
    for case in 0..6000 {
        let (hot, cold) = (gen_hot(&mut r).path(), gen_cold(&mut r).path());
        let junk: String = (0..r.range(1, 12)).map(|_| *r.pick(TOKENS)).collect();
        let lossy = String::from_utf8_lossy(&r.vec(0, 40)).into_owned();
        for text in [
            mutate_str(&mut r, &hot),
            mutate_str(&mut r, &cold),
            junk,
            lossy,
        ] {
            if let Some(n) = HotName::parse(&text) {
                hot_ok += 1;
                assert_eq!(n.path(), text, "case {case}");
            }
            if let Some(n) = ColdName::parse(&text) {
                cold_ok += 1;
                assert_eq!(n.path(), text, "case {case}");
            }
        }
    }
    assert!(
        hot_ok > 0 && cold_ok > 0,
        "no mutated name parsed, the check is vacuous"
    );
}

#[test]
fn names_with_a_second_spelling_are_not_names() {
    let id = 0x0000_0000_0000_0abc_u64;
    let upper = format!("seg/{id:016X}.seg");
    assert_eq!(HotName::parse(&upper), None);
    assert_eq!(
        HotName::parse("cache/1.2.seg"),
        Some(HotName::Cache { boot: 1, seq: 2 })
    );
    for bad in [
        "cache/01.2.seg",
        "cache/+1.2.seg",
        "cache/1.2.3.seg",
        "cache/1..seg",
        "cache/.2.seg",
    ] {
        assert_eq!(HotName::parse(bad), None, "{bad}");
    }
    let origin = |p: &str| HotName::parse(p);
    assert_eq!(
        origin("tomb/255/1.log"),
        Some(HotName::TombLog {
            origin: NodeId(255),
            first_seq: 1
        })
    );
    for bad in [
        "tomb/256/1.log",
        "tomb/007/1.log",
        "tomb/7/0001.log",
        "tomb//1.log",
        "tomb/7/18446744073709551616.log",
    ] {
        assert_eq!(HotName::parse(bad), None, "{bad}");
    }
    let mut r = Rng(25);
    for _ in 0..100 {
        let f = LargeFile {
            key_hash: KeyHash(r.array()),
            generation: Generation(r.array()),
            size: r.u64(),
        };
        let path = HotName::Large(f).path();
        let (dir, _) = path[4..].split_once('/').unwrap();
        let other = format!("{:02x}", u8::from_str_radix(dir, 16).unwrap() ^ 1);
        assert_eq!(HotName::parse(&path.replacen(dir, &other, 1)), None);
        let cold = gen_cold(&mut r).path();
        let (dir, _) = cold[2..].split_once('/').unwrap();
        let other = format!("{:02x}", u8::from_str_radix(dir, 16).unwrap() ^ 1);
        assert_eq!(ColdName::parse(&cold.replacen(dir, &other, 1)), None);
    }
}

#[test]
fn identity_files_round_trip_and_only_the_canonical_text_decodes() {
    let mut r = Rng(26);
    for case in 0..500 {
        let node = NodeIdentity {
            cluster: ClusterId(r.array()),
            node: NodeId(r.u8()),
        };
        let drive = DriveIdentity {
            cluster: ClusterId(r.array()),
            node: NodeId(r.u8()),
            drive: DriveId(r.array()),
            filesystem: DriveId(r.array()),
        };
        let (nt, dt) = (node.encode(), drive.encode());
        assert_eq!(NodeIdentity::decode(&nt), Ok(node), "case {case}");
        assert_eq!(DriveIdentity::decode(&dt), Ok(drive), "case {case}");
        for cut in 0..nt.len() {
            assert!(
                NodeIdentity::decode(&nt[..cut]).is_err(),
                "case {case} cut {cut}"
            );
        }
        for cut in 0..dt.len() {
            assert!(
                DriveIdentity::decode(&dt[..cut]).is_err(),
                "case {case} cut {cut}"
            );
        }
        for text in [
            nt.replace('\n', "\r\n"),
            format!("{nt}x\n"),
            format!("\n{nt}"),
        ] {
            assert!(NodeIdentity::decode(&text).is_err());
        }
        for _ in 0..10 {
            let text = mutate_str(&mut r, &nt);
            if let Ok(n) = NodeIdentity::decode(&text) {
                assert_eq!(n.encode(), text);
            }
            let text = mutate_str(&mut r, &dt);
            if let Ok(d) = DriveIdentity::decode(&text) {
                assert_eq!(d.encode(), text);
            }
        }
        let want = FormatError::UnsupportedVersion {
            what: "NODE file",
            version: 2,
        };
        assert_eq!(
            NodeIdentity::decode(&nt.replacen("= 1", "= 2", 1)),
            Err(want)
        );
        let want = FormatError::UnsupportedVersion {
            what: "DRIVE file",
            version: 2,
        };
        assert_eq!(
            DriveIdentity::decode(&dt.replacen("= 1", "= 2", 1)),
            Err(want)
        );
        assert_eq!(
            NodeIdentity::decode(&dt),
            Err(invalid("NODE file", "a line after the last field"))
        );
        assert_eq!(
            DriveIdentity::decode(&nt),
            Err(invalid("DRIVE file", "a field is missing"))
        );
        let text = nt.replacen(&format!("node = {}", node.node), "node = 256", 1);
        let want = invalid("NODE file", "a line is not the expected `key = value`");
        assert_eq!(NodeIdentity::decode(&text), Err(want));
    }
    for _ in 0..2000 {
        let n = r.range(0, 120);
        let text = String::from_utf8_lossy(&r.bytes(n)).into_owned();
        assert!(NodeIdentity::decode(&text).is_err() && DriveIdentity::decode(&text).is_err());
    }
}

#[test]
fn id_texts_round_trip_and_reject_wrong_lengths_and_non_lowercase_hex() {
    let mut r = Rng(27);
    let digits = "wrong number of hex digits";
    for case in 0..1500 {
        let raw = r.u64();
        let (b, s) = (BlockId::from_raw(raw), StripeId::from_raw(r.u64()));
        assert_eq!(b.to_string().parse(), Ok(b), "case {case}");
        assert_eq!(s.to_string().parse(), Ok(s));
        assert_eq!(b.to_string().len(), 16);
        let (d, c) = (DriveId(r.array()), ClusterId(r.array()));
        let (g, k) = (Generation(r.array()), KeyHash(r.array()));
        assert_eq!(d.to_string().parse(), Ok(d));
        assert_eq!(c.to_string().parse(), Ok(c));
        assert_eq!(g.to_string().parse(), Ok(g));
        assert_eq!(k.to_string().parse(), Ok(k));
        assert_eq!(g.to_string().len(), 32);
        assert_eq!(
            b.to_string().to_uppercase().parse::<BlockId>().is_ok(),
            !b.to_string()
                .contains(|c: char| c.is_ascii_lowercase() && c > '9')
        );
        let text = b.to_string();
        for bad in [&text[1..], &format!("{text}0")[..], ""] {
            assert_eq!(bad.parse::<BlockId>(), Err(invalid("block id", digits)));
            assert_eq!(bad.parse::<StripeId>(), Err(invalid("stripe id", digits)));
            assert_eq!(
                bad.parse::<Generation>(),
                Err(invalid("generation", digits))
            );
        }
        let mut chars: Vec<char> = g.to_string().chars().collect();
        let at = r.range(0, 31);
        chars[at] = *r.pick(&['G', 'A', 'F', 'g', ' ', '-', '+', 'x']);
        let bad: String = chars.into_iter().collect();
        let want = invalid("key hash", "not a lowercase hex digit");
        assert_eq!(bad.parse::<KeyHash>(), Err(want));
        let junk = String::from_utf8_lossy(&r.vec(0, 40)).into_owned();
        let _ = (
            junk.parse::<BlockId>(),
            junk.parse::<DriveId>(),
            junk.parse::<ClusterId>(),
        );
    }
}

#[test]
fn ids_and_stamps_round_trip_through_json_and_refuse_bad_json() {
    let mut r = Rng(28);
    for case in 0..800 {
        let b = BlockId::from_raw(r.u64());
        let text = serde_json::to_string(&b).unwrap();
        assert_eq!(text, format!("\"{b}\""));
        assert_eq!(
            serde_json::from_str::<BlockId>(&text).unwrap(),
            b,
            "case {case}"
        );
        let g = Generation(r.array());
        let text = serde_json::to_string(&g).unwrap();
        assert_eq!(serde_json::from_str::<Generation>(&text).unwrap(), g);
        let stamp = Stamp::from_raw(r.u64());
        let text = serde_json::to_string(&stamp).unwrap();
        assert_eq!(text, stamp.raw().to_string());
        assert_eq!(serde_json::from_str::<Stamp>(&text).unwrap(), stamp);
        let ballot = Ballot {
            round: r.u32(),
            node: NodeId(r.u8()),
        };
        let text = serde_json::to_string(&ballot).unwrap();
        assert_eq!(serde_json::from_str::<Ballot>(&text).unwrap(), ballot);
        let p = SealProposal {
            length: r.u64(),
            running_crc: r.u32(),
        };
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<SealProposal>(&text).unwrap(), p);
        let class = ExpiryClass(r.u8());
        let text = serde_json::to_string(&class).unwrap();
        assert_eq!(serde_json::from_str::<ExpiryClass>(&text).unwrap(), class);
        assert!(serde_json::from_str::<BlockId>(&format!("\"{}\"", &b.to_string()[1..])).is_err());
        assert!(serde_json::from_str::<BlockId>(&r.u64().to_string()).is_err());
        assert!(
            serde_json::from_str::<Generation>(&format!(
                "\"{}\"",
                g.to_string().to_uppercase() + "G"
            ))
            .is_err()
        );
        let wall = r.u64() & WALL_MAX;
        let s = Stamp::from_parts(wall, r.u16()).unwrap();
        assert_eq!(s.wall_ms(), wall);
        assert_eq!(Stamp::from_parts(s.wall_ms(), s.counter()), Ok(s));
        let node = NodeId(r.u8());
        let counter = r.u64() & ((1 << 56) - 1);
        let id = BlockId::new(node, counter).unwrap();
        assert_eq!((id.node(), id.counter()), (node, counter));
        assert_eq!(
            BlockId::new(node, counter | 1 << 56),
            Err(invalid("block id", "counter above 2^56 - 1"))
        );
        assert_eq!(
            Stamp::from_parts(WALL_MAX + 1 + r.below(100), 0),
            Err(invalid("stamp", "wall clock above 2^48 - 1 ms"))
        );
    }
}
