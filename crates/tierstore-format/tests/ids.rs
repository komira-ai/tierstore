//! The ids, the stamp and the expiry class: how they are built from their parts, how they print and
//! parse, and how they travel as JSON.
//!
//! Catches:
//! - a block or stripe id whose node and counter are not the high 8 and low 56 bits, or a counter
//!   above 2^56 - 1 that is accepted and silently overlaps the node bits;
//! - a text form that is not exactly the lowercase hex digits of the id, or a parse that accepts a
//!   wrong length, an uppercase digit, a non-hex digit or a sign;
//! - an id or a stamp that is not a string (id) or a number (stamp, node, expiry class) in JSON, or
//!   a JSON string that is not an id and is still taken as one;
//! - a stamp whose wall clock above 2^48 - 1 ms is accepted, or whose order is not wall first, then
//!   counter.

use tierstore_format::FormatError;
use tierstore_format::ids::{
    BlockId, COUNTER_MAX, ClusterId, DriveId, ExpiryClass, Generation, KeyHash, NodeId, StripeId,
};
use tierstore_format::stamp::{STAMP_COUNTER_BITS, Stamp, WALL_BITS, WALL_MAX};

fn is_invalid(e: &FormatError, what: &str) -> bool {
    matches!(e, FormatError::Invalid { what: w, .. } if *w == what)
}

#[test]
fn a_block_id_holds_the_node_in_the_high_bits_and_the_counter_in_the_low_56() {
    let id = BlockId::new(NodeId(0xab), 5).unwrap();
    assert_eq!(id.raw(), 0xab00_0000_0000_0005);
    assert_eq!(id.node(), NodeId(0xab));
    assert_eq!(id.counter(), 5);
    assert_eq!(BlockId::from_raw(id.raw()), id);
}

#[test]
fn the_largest_counter_fits_and_one_more_is_refused_for_blocks_and_stripes() {
    assert_eq!(COUNTER_MAX, (1u64 << 56) - 1);
    let top = BlockId::new(NodeId(1), COUNTER_MAX).unwrap();
    assert_eq!(top.raw(), 0x01ff_ffff_ffff_ffff);
    assert_eq!(top.counter(), COUNTER_MAX);
    assert_eq!(top.node(), NodeId(1));
    let err = BlockId::new(NodeId(1), COUNTER_MAX + 1).unwrap_err();
    assert!(is_invalid(&err, "block id"));
    assert_eq!(err.to_string(), "block id: counter above 2^56 - 1");
    let err = StripeId::new(NodeId(1), COUNTER_MAX + 1).unwrap_err();
    assert_eq!(err.to_string(), "stripe id: counter above 2^56 - 1");
    assert_eq!(
        StripeId::new(NodeId(255), 0).unwrap().raw(),
        0xff00_0000_0000_0000
    );
    assert_eq!(BlockId::new(NodeId(0), 0).unwrap().raw(), 0);
}

#[test]
fn from_raw_takes_every_u64_and_node_and_counter_read_it_back() {
    let id = StripeId::from_raw(u64::MAX);
    assert_eq!(id.node(), NodeId(255));
    assert_eq!(id.counter(), COUNTER_MAX);
    assert_eq!(StripeId::from_raw(0).node(), NodeId(0));
}

#[test]
fn ids_order_by_their_raw_value() {
    let a = BlockId::new(NodeId(1), 9).unwrap();
    let b = BlockId::new(NodeId(2), 1).unwrap();
    assert!(a < b);
    assert!(NodeId(3) < NodeId(4));
}

#[test]
fn an_allocated_id_prints_as_sixteen_lowercase_hex_digits_and_parses_back() {
    let id = BlockId::from_raw(0x00ab_cdef_0123_4567);
    assert_eq!(id.to_string(), "00abcdef01234567");
    assert_eq!("00abcdef01234567".parse::<BlockId>().unwrap(), id);
    assert_eq!(BlockId::from_raw(0).to_string(), "0000000000000000");
    assert_eq!(
        "ffffffffffffffff".parse::<StripeId>().unwrap(),
        StripeId::from_raw(u64::MAX)
    );
    assert_eq!(StripeId::from_raw(0x1f).to_string(), "000000000000001f");
}

#[test]
fn a_node_id_prints_as_a_decimal_number() {
    assert_eq!(NodeId(0).to_string(), "0");
    assert_eq!(NodeId(255).to_string(), "255");
}

#[test]
fn an_id_text_of_the_wrong_length_is_refused() {
    for text in ["", "0", "000000000000000", "00000000000000000"] {
        let err = text.parse::<BlockId>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "block id: wrong number of hex digits",
            "{text:?}"
        );
    }
    let err = "0".repeat(31).parse::<DriveId>().unwrap_err();
    assert_eq!(err.to_string(), "drive id: wrong number of hex digits");
    let err = "0".repeat(33).parse::<Generation>().unwrap_err();
    assert_eq!(err.to_string(), "generation: wrong number of hex digits");
    let err = "0".repeat(16).parse::<KeyHash>().unwrap_err();
    assert_eq!(err.to_string(), "key hash: wrong number of hex digits");
}

#[test]
fn an_uppercase_or_non_hex_or_signed_id_text_is_refused() {
    for text in [
        "00ABCDEF01234567",
        "00abcdef0123456g",
        "+0abcdef01234567",
        "-0abcdef01234567",
        "0x abcdef0123456",
        "00abcdef0123456 ",
    ] {
        let err = text.parse::<StripeId>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "stripe id: not a lowercase hex digit",
            "{text:?}"
        );
    }
    // The characters either side of each accepted range.
    for bad in ["/", ":", "@", "`", "g", "F", "A"] {
        let text = format!("{}{}", "0".repeat(15), bad);
        assert!(text.parse::<BlockId>().is_err(), "{text:?}");
    }
    for ok in ["0", "9", "a", "f"] {
        let text = format!("{}{}", "0".repeat(15), ok);
        assert!(text.parse::<BlockId>().is_ok(), "{text:?}");
    }
    // A multi-byte character is not a digit and does not split a pair into a panic.
    let err = format!("{}é", "0".repeat(14))
        .parse::<BlockId>()
        .unwrap_err();
    assert!(is_invalid(&err, "block id"));
}

#[test]
fn a_sixteen_byte_id_prints_as_thirty_two_hex_digits_and_parses_back() {
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u8) * 17;
    }
    let text = "00112233445566778899aabbccddeeff";
    assert_eq!(DriveId(bytes).to_string(), text);
    assert_eq!(ClusterId(bytes).to_string(), text);
    assert_eq!(Generation(bytes).to_string(), text);
    assert_eq!(KeyHash(bytes).to_string(), text);
    assert_eq!(text.parse::<DriveId>().unwrap(), DriveId(bytes));
    assert_eq!(text.parse::<ClusterId>().unwrap(), ClusterId(bytes));
    assert_eq!(text.parse::<Generation>().unwrap(), Generation(bytes));
    assert_eq!(text.parse::<KeyHash>().unwrap(), KeyHash(bytes));
    assert!(text.to_uppercase().parse::<KeyHash>().is_err());
    assert_eq!(DriveId([0; 16]).to_string(), "0".repeat(32));
}

#[test]
fn ids_are_json_strings_in_their_text_form() {
    let block = BlockId::from_raw(0xab00_0000_0000_0005);
    assert_eq!(
        serde_json::to_string(&block).unwrap(),
        "\"ab00000000000005\""
    );
    assert_eq!(
        serde_json::from_str::<BlockId>("\"ab00000000000005\"").unwrap(),
        block
    );
    let stripe = StripeId::from_raw(7);
    assert_eq!(
        serde_json::to_string(&stripe).unwrap(),
        "\"0000000000000007\""
    );
    let generation = Generation([0xab; 16]);
    let json = serde_json::to_string(&generation).unwrap();
    assert_eq!(json, format!("\"{}\"", "ab".repeat(16)));
    assert_eq!(
        serde_json::from_str::<Generation>(&json).unwrap(),
        generation
    );
    let cluster = ClusterId([1; 16]);
    let json = serde_json::to_string(&cluster).unwrap();
    assert_eq!(serde_json::from_str::<ClusterId>(&json).unwrap(), cluster);
    // An owned string (a JSON value, an escaped string) reads the same as a borrowed one.
    let value = serde_json::Value::String("ab00000000000005".to_owned());
    assert_eq!(serde_json::from_value::<BlockId>(value).unwrap(), block);
    assert_eq!(
        serde_json::from_str::<BlockId>("\"\\u0061b00000000000005\"").unwrap(),
        block
    );
}

#[test]
fn a_json_value_that_is_not_an_id_text_is_refused_with_the_parse_error() {
    let err = serde_json::from_str::<BlockId>("\"AB00000000000005\"").unwrap_err();
    assert!(
        err.to_string()
            .contains("block id: not a lowercase hex digit"),
        "{err}"
    );
    let err = serde_json::from_str::<DriveId>("\"00\"").unwrap_err();
    assert!(
        err.to_string()
            .contains("drive id: wrong number of hex digits"),
        "{err}"
    );
    assert!(serde_json::from_str::<BlockId>("12345").is_err());
    assert!(serde_json::from_str::<BlockId>("null").is_err());
}

#[test]
fn a_node_id_and_an_expiry_class_are_json_numbers() {
    assert_eq!(serde_json::to_string(&NodeId(9)).unwrap(), "9");
    assert_eq!(serde_json::from_str::<NodeId>("255").unwrap(), NodeId(255));
    assert!(serde_json::from_str::<NodeId>("256").is_err());
    assert_eq!(serde_json::to_string(&ExpiryClass(3)).unwrap(), "3");
    assert_eq!(
        serde_json::from_str::<ExpiryClass>("0").unwrap(),
        ExpiryClass::default()
    );
    assert_eq!(ExpiryClass::default(), ExpiryClass(0));
}

#[test]
fn a_stamp_holds_wall_milliseconds_in_the_high_48_bits_and_the_counter_in_the_low_16() {
    assert_eq!(WALL_BITS, 48);
    assert_eq!(STAMP_COUNTER_BITS, 16);
    let s = Stamp::from_parts(1_700_000_000_123, 42).unwrap();
    assert_eq!(s.raw(), (1_700_000_000_123u64 << 16) | 42);
    assert_eq!(s.wall_ms(), 1_700_000_000_123);
    assert_eq!(s.counter(), 42);
    assert_eq!(Stamp::from_raw(s.raw()), s);
    assert_eq!(Stamp::ZERO.raw(), 0);
    assert_eq!(Stamp::default(), Stamp::ZERO);
}

#[test]
fn a_stamp_accepts_a_wall_clock_up_to_2_pow_48_minus_1_and_refuses_one_more() {
    assert_eq!(WALL_MAX, (1u64 << 48) - 1);
    let top = Stamp::from_parts(WALL_MAX, u16::MAX).unwrap();
    assert_eq!(top.raw(), u64::MAX);
    assert_eq!(top.wall_ms(), WALL_MAX);
    assert_eq!(top.counter(), u16::MAX);
    let err = Stamp::from_parts(WALL_MAX + 1, 0).unwrap_err();
    assert_eq!(err.to_string(), "stamp: wall clock above 2^48 - 1 ms");
    assert!(Stamp::from_parts(u64::MAX, 0).is_err());
    assert_eq!(Stamp::from_parts(0, 0).unwrap(), Stamp::ZERO);
}

#[test]
fn stamps_order_by_wall_milliseconds_then_by_counter() {
    let a = Stamp::from_parts(100, 65535).unwrap();
    let b = Stamp::from_parts(101, 0).unwrap();
    let c = Stamp::from_parts(101, 1).unwrap();
    assert!(a < b);
    assert!(b < c);
    let mut all = vec![c, a, b];
    all.sort();
    assert_eq!(all, vec![a, b, c]);
}

#[test]
fn a_stamp_is_a_json_number_of_its_u64() {
    let s = Stamp::from_parts(3, 4).unwrap();
    assert_eq!(
        serde_json::to_string(&s).unwrap(),
        ((3u64 << 16) | 4).to_string()
    );
    assert_eq!(serde_json::from_str::<Stamp>("196612").unwrap(), s);
    assert!(serde_json::from_str::<Stamp>("\"196612\"").is_err());
}
