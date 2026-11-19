//! `xl.meta`: a real file from a silo drive, and files made here for every branch.

use std::path::Path;

use super::*;

/// The `xl.meta` of a 45,481,624-byte object written at 7 + 5 (silo raised the parity from 4
/// to 5 at the write: `x-minio-internal-erasure-upgraded`), not inlined, one part, one version,
/// as read from a drive of silo.
const REAL: &str = "584c322001000300c60000018a030301c42697c41000000000000000000000000000000000d318dde9f77813e71ac40456f6d5cd01020507c5015c83a45479706501a556324f626ade0011a24944c41000000000000000000000000000000000a444446972c4101fbc8984408d42c7a2137fc80616214ba64563416c676f01a345634d07a345634e05a745634253697a65d200100000a74563496e64657809a64563446973749c0405060708090a0b0c010203a84353756d416c676f01a8506172744e756d739101a9506172744554616773c0a95061727453697a657391d202b5fe98aa506172744153697a657391d202b5fe98a453697a65d202b5fe98a54d54696d65d318dde9f77813e71aa74d65746153797381d921782d6d696e696f2d696e7465726e616c2d657261737572652d7570677261646564c404342d3e35a74d65746155737282ac636f6e74656e742d74797065b362696e6172792f6f637465742d73747265616da465746167d9203064646135326337303762366166633733376539643731616365316130333335a176ce68bdaf15ced58f0348";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A msgpack encoder for the tests: the smallest form of each value, or a forced wide form.
#[derive(Default)]
pub(crate) struct Enc(pub(crate) Vec<u8>);

impl Enc {
    pub(crate) fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    pub(crate) fn str(&mut self, s: &str) -> &mut Self {
        let n = s.len();
        if n < 32 {
            self.0.push(0xa0 | n as u8);
        } else {
            self.0.extend_from_slice(&[0xd9, n as u8]);
        }
        self.raw(s.as_bytes())
    }
    pub(crate) fn bin(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(&[0xc5]);
        self.0.extend_from_slice(&(b.len() as u16).to_be_bytes());
        self.raw(b)
    }
    pub(crate) fn int(&mut self, i: i64) -> &mut Self {
        if (0..128).contains(&i) {
            self.0.push(i as u8);
        } else {
            self.0.push(0xd3);
            self.0.extend_from_slice(&i.to_be_bytes());
        }
        self
    }
    pub(crate) fn map(&mut self, n: u8) -> &mut Self {
        self.0.push(0x80 | n);
        self
    }
}

/// An `xl.meta` whose newest version's metadata is `version` (msgpack), with `count` versions.
pub(crate) fn xl(version: &[u8], count: i64) -> Vec<u8> {
    let mut block = Enc::default();
    block.int(3).int(3).int(count);
    for _ in 0..count.max(1) {
        block.bin(&[0x90]).bin(version);
    }
    let mut out = b"XL2 \x01\x00\x03\x00".to_vec();
    let mut outer = Enc::default();
    outer.0.push(0xc6);
    outer
        .0
        .extend_from_slice(&(block.0.len() as u32).to_be_bytes());
    outer.raw(&block.0);
    out.extend_from_slice(&outer.0);
    out.extend_from_slice(&[0xce, 1, 2, 3, 4]);
    out
}

/// The `V2Obj` of an object: size, time, shards, parts, inline flag, tag.
pub(crate) fn object(size: i64, inline: bool, parts: u8) -> Vec<u8> {
    let mut e = Enc::default();
    e.map(2).str("Type").int(1).str("V2Obj").map(7);
    e.str("Size")
        .int(size)
        .str("MTime")
        .int(1_790_000_000_123_456_789);
    e.str("EcM").int(8).str("EcN").int(4);
    e.str("PartNums").raw(&[0x90 | parts]);
    for p in 0..parts {
        e.int(i64::from(p) + 1);
    }
    e.str("MetaSys").map(u8::from(inline));
    if inline {
        e.str("x-minio-internal-inline-data").bin(b"true");
    }
    e.str("MetaUsr").map(1).str("etag").str("abc-2");
    e.0
}

/// The real file from silo reads as its listing says: size, time, tag; 7 data and 5 parity
/// shards; one part, not inlined, one version.
#[test]
fn a_real_xl_meta_from_silo_reads_as_its_listing_says() {
    let meta = parse(&unhex(REAL), Path::new("xl.meta")).unwrap();
    assert_eq!(
        meta,
        Meta::Object(ObjectMeta {
            size: 45_481_624,
            modified_ms: 1_791_845_475_844,
            etag: "0dda52c707b6afc737e9d71ace1a0335".to_owned(),
            data: 7,
            parity: 5,
            parts: 1,
            inline: false,
            versions: 1,
        })
    );
}

/// An inlined multipart object with two versions; a delete marker as the newest version is
/// `Deleted`.
#[test]
fn inline_parts_versions_and_delete_markers_are_read() {
    let meta = parse(&xl(&object(300, true, 2), 2), Path::new("x")).unwrap();
    let Meta::Object(o) = meta else { panic!() };
    assert_eq!(
        (
            o.size,
            o.modified_ms,
            o.inline,
            o.parts,
            o.versions,
            o.etag.as_str()
        ),
        (300, 1_790_000_000_123, true, 2, 2, "abc-2")
    );
    let mut marker = Enc::default();
    marker.map(1).str("Type").int(2);
    assert_eq!(
        parse(&xl(&marker.0, 1), Path::new("x")).unwrap(),
        Meta::Deleted { versions: 1 }
    );
    // An inline flag written as a string, and no PartNums: one part.
    let mut e = Enc::default();
    e.map(2).str("Type").int(1).str("V2Obj").map(6);
    e.str("Size")
        .int(0)
        .str("MTime")
        .int(0)
        .str("EcM")
        .int(4)
        .str("EcN")
        .int(2);
    e.str("MetaSys")
        .map(1)
        .str("x-minio-internal-inline-data")
        .str("true");
    e.str("MetaUsr")
        .map(1)
        .str("etag")
        .str("d41d8cd98f00b204e9800998ecf8427e");
    let Meta::Object(o) = parse(&xl(&e.0, 1), Path::new("x")).unwrap() else {
        panic!()
    };
    assert_eq!((o.inline, o.parts, o.data, o.parity), (true, 1, 4, 2));

    // The flag written as bytes other than `true` is not inlined.
    let mut e = Enc::default();
    e.map(2).str("Type").int(1).str("V2Obj").map(6);
    e.str("Size")
        .int(0)
        .str("MTime")
        .int(0)
        .str("EcM")
        .int(4)
        .str("EcN")
        .int(2);
    e.str("MetaSys")
        .map(1)
        .str("x-minio-internal-inline-data")
        .bin(b"false");
    e.str("MetaUsr").map(1).str("etag").str("e");
    let Meta::Object(o) = parse(&xl(&e.0, 1), Path::new("x")).unwrap() else {
        panic!()
    };
    assert!(!o.inline);
}

/// Each way a file is not a readable `xl.meta` is refused with a reason naming it.
#[test]
fn a_damaged_xl_meta_is_refused_with_its_reason() {
    let why = |bytes: &[u8]| parse(bytes, Path::new("m")).unwrap_err().to_string();
    assert_eq!(
        why(b"XL1 \x01\x00\x03\x00"),
        "m:0: not an xl.meta (no XL2 magic)"
    );
    assert_eq!(why(b"XL2"), "m:0: not an xl.meta (no XL2 magic)");
    assert_eq!(
        why(b"XL2 \x02\x00\x03\x00\xc0"),
        "m:0: xl.meta major version 2, not 1"
    );
    assert_eq!(
        why(b"XL2 \x01\x00\x03\x00\xc0"),
        "m:0: the version block is not a bin"
    );
    assert!(why(b"XL2 \x01\x00\x03\x00\xc6\x00\x00\x00\x09").contains("past the end"));
    // A block that ends early, a count that is not one, no version, metadata not a bin.
    let block = |b: &[u8]| {
        let mut out = b"XL2 \x01\x00\x03\x00\xc4".to_vec();
        out.push(b.len() as u8);
        out.extend_from_slice(b);
        out
    };
    assert_eq!(
        why(&block(&[3])),
        "m:0: meta version: 0 bytes wanted at offset 1, past the end".replace("0 bytes", "1 bytes")
    );
    assert_eq!(
        why(&block(&[3, 3, 0xc0])),
        "m:0: the version count is not a count"
    );
    assert_eq!(why(&block(&[3, 3, 0])), "m:0: no version");
    assert_eq!(
        why(&block(&[3, 3, 1, 0x90, 0x90])),
        "m:0: the version metadata is not a bin"
    );
    assert!(why(&block(&[3, 3, 1, 0x90])).starts_with("m:0: version metadata: "));
    assert!(why(&block(&[3, 3, 1])).starts_with("m:0: version header: "));
    assert!(why(&block(&[3, 3])).starts_with("m:0: version count: "));
    assert!(why(&block(&[])).starts_with("m:0: header version: "));
    // Metadata that is not msgpack, a type that is not an object, no V2Obj.
    assert!(why(&xl(&[0xc1], 1)).contains("0xc1 is not a msgpack type"));
    let mut e = Enc::default();
    e.map(1).str("Type").int(3);
    assert_eq!(
        why(&xl(&e.0, 1)),
        "m:0: version type Some(3), not an object"
    );
    let mut e = Enc::default();
    e.map(1).str("Type").int(1);
    assert_eq!(why(&xl(&e.0, 1)), "m:0: an object version without V2Obj");
    // A V2Obj without a size, with a negative size or time, shards too large, no etag.
    let v2 = |fields: &dyn Fn(&mut Enc), n: u8| {
        let mut e = Enc::default();
        e.map(2).str("Type").int(1).str("V2Obj").map(n);
        fields(&mut e);
        xl(&e.0, 1)
    };
    assert_eq!(
        why(&v2(
            &|e| {
                e.str("MTime").int(0);
            },
            1
        )),
        "m:0: V2Obj without an integer Size"
    );
    assert!(
        why(&v2(
            &|e| {
                e.str("Size").int(-1);
            },
            1
        ))
        .starts_with("m:0: Size: ")
    );
    assert!(
        why(&v2(
            &|e| {
                e.str("Size").int(1).str("MTime").int(-5);
            },
            2
        ))
        .starts_with("m:0: MTime: ")
    );
    assert!(
        why(&v2(
            &|e| {
                e.str("Size")
                    .int(1)
                    .str("MTime")
                    .int(1)
                    .str("EcM")
                    .int(i64::MAX);
                e.str("MetaUsr").map(1).str("etag").str("t");
            },
            4
        ))
        .starts_with("m:0: EcM: ")
    );
    assert_eq!(
        why(&v2(
            &|e| {
                e.str("Size").int(1).str("MTime").int(1);
            },
            2
        )),
        "m:0: V2Obj without MetaUsr etag"
    );
}

/// Every msgpack type decodes to its value and length.
#[test]
fn every_msgpack_type_decodes() {
    let d = |b: &[u8]| decode(b).unwrap();
    assert_eq!(d(&[0x05]), (Value::Int(5), 1));
    assert_eq!(d(&[0xff]), (Value::Int(-1), 1));
    assert_eq!(d(&[0xc0]), (Value::Nil, 1));
    assert_eq!(d(&[0xc2]), (Value::Bool(false), 1));
    assert_eq!(d(&[0xc3]), (Value::Bool(true), 1));
    assert_eq!(d(&[0xa1, b'x']), (Value::Str("x".into()), 2));
    assert_eq!(d(&[0xd9, 1, b'y']), (Value::Str("y".into()), 3));
    assert_eq!(d(&[0xda, 0, 1, b'z']), (Value::Str("z".into()), 4));
    assert_eq!(d(&[0xdb, 0, 0, 0, 0]), (Value::Str(String::new()), 5));
    assert_eq!(d(&[0xc4, 1, 9]), (Value::Bin(vec![9]), 3));
    assert_eq!(d(&[0xc6, 0, 0, 0, 0]), (Value::Bin(vec![]), 5));
    assert_eq!(d(&[0xc7, 1, 0xfe, 7]), (Value::Ext(-2, vec![7]), 4));
    assert_eq!(d(&[0xd4, 3, 1]), (Value::Ext(3, vec![1]), 3));
    assert_eq!(
        d(&[0xd8, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).1,
        18
    );
    assert_eq!(d(&[0xca, 0x3f, 0x80, 0, 0]), (Value::Float(1.0), 5));
    assert_eq!(
        d(&[0xcb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0]),
        (Value::Float(1.0), 9)
    );
    assert_eq!(d(&[0xcc, 200]), (Value::Int(200), 2));
    assert_eq!(
        d(&[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).0,
        Value::Int(i128::from(u64::MAX))
    );
    assert_eq!(d(&[0xd0, 0x80]), (Value::Int(-128), 2));
    assert_eq!(d(&[0xd1, 0xff, 0xfe]), (Value::Int(-2), 3));
    assert_eq!(
        d(&[0x92, 1, 2]),
        (Value::Array(vec![Value::Int(1), Value::Int(2)]), 3)
    );
    assert_eq!(d(&[0xdc, 0, 1, 7]), (Value::Array(vec![Value::Int(7)]), 4));
    assert_eq!(d(&[0xdd, 0, 0, 0, 0]), (Value::Array(vec![]), 5));
    assert_eq!(d(&[0xde, 0, 0]), (Value::Map(vec![]), 3));
    assert_eq!(d(&[0xdf, 0, 0, 0, 0]), (Value::Map(vec![]), 5));
    let (m, _) = d(&[0x81, 0xa1, b'k', 0x01]);
    assert_eq!(m.get("k"), Some(&Value::Int(1)));
    assert_eq!(m.get("absent"), None);
    assert_eq!(Value::Nil.get("k"), None);
    assert_eq!(Value::Nil.int(), None);
    assert_eq!(Value::Nil.str(), None);
    // A map whose key is not a string is skipped by `get`.
    let (m, _) = d(&[0x81, 0x01, 0x02]);
    assert_eq!(m.get("1"), None);
}

/// Values cut short, strings that are not UTF-8, lengths past the end and nesting too deep are
/// errors, not panics.
#[test]
fn damaged_msgpack_is_an_error() {
    assert!(decode(&[]).is_err());
    assert!(decode(&[0xa2, b'x']).is_err());
    assert!(decode(&[0xa1, 0xff]).unwrap_err().contains("utf-8"));
    assert!(
        decode(&[0xc5, 0xff, 0xff])
            .unwrap_err()
            .contains("past the end")
    );
    assert!(decode(&[0x91]).is_err());
    assert!(decode(&[0x81, 0x01]).is_err());
    assert!(decode(&[0xc7, 2, 1]).is_err());
    assert!(decode(&[0xc7]).is_err());
    assert_eq!(decode(&[0x91; 40]).unwrap_err(), "nested deeper than 16");
}
