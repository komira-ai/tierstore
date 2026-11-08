use super::*;
use crate::channels::rpm::testing::*;
use crate::release::rpm as build;

fn package() -> Vec<u8> {
    build::build("1.2.3", "x86_64", &build::files(b"binary", b"notice")).expect("build")
}

#[test]
fn a_built_package_is_read_and_written_again_byte_for_byte() {
    // Catches: a signature header written in another form than `release rpm` builds, which would
    // make the signed package differ from the attested one by more than the signature.
    let bytes = package();
    let parts = split(&bytes).expect("parts");
    assert_eq!(parts.assemble(), bytes);
    assert!(parts.signature().is_none());
    let tags: Vec<u32> = parts.sig.entries.iter().map(|e| e.tag).collect();
    assert!(
        tags.contains(&269) && tags.contains(&273) && tags.contains(&1000),
        "{tags:?}"
    );
    let (start, end) = parts.header_range();
    assert_eq!(&bytes[start..end], parts.header);
    assert_eq!(&bytes[end..], parts.payload);
}

#[test]
fn a_signature_is_added_to_the_signature_header_alone_and_taking_it_out_gives_the_package_back() {
    // Catches: a signature that changes the main header or the payload (the signature would not
    // fit what it signs), one that is not found again, and a package that is not the attested one
    // once the signature is removed.
    let bytes = package();
    let parts = split(&bytes).expect("parts");
    let sig = packet(1, FPR, b"mpi");
    let signed = with_signature(&bytes, TAG_RSA, &sig).expect("signed");
    let again = split(&signed).expect("signed parts");
    assert_eq!(again.signature(), Some((TAG_RSA, sig.as_slice())));
    assert_eq!(again.header, parts.header);
    assert_eq!(again.payload, parts.payload);
    assert_eq!(again.sig.entries.len(), parts.sig.entries.len() + 1);
    assert!(signed.len() > bytes.len());
    assert_eq!(without_signatures(&signed).expect("stripped"), bytes);
    assert_eq!(without_signatures(&bytes).expect("unsigned"), bytes);
    let (start, end) = again.header_range();
    assert_eq!(&signed[start..end], parts.header);
    let dsa = with_signature(&bytes, TAG_DSA, &sig).expect("dsa");
    assert_eq!(
        split(&dsa).expect("dsa parts").signature().map(|s| s.0),
        Some(TAG_DSA)
    );
    assert_eq!(without_signatures(&dsa).expect("stripped"), bytes);
}

#[test]
fn a_package_that_is_signed_already_is_refused() {
    // Catches: a second signature stacked on the first, which no reader would take.
    let signed = with_signature(&package(), TAG_RSA, &packet(1, FPR, b"x")).expect("signed");
    let e = with_signature(&signed, TAG_RSA, &packet(1, FPR, b"y")).unwrap_err();
    assert!(e.to_string().contains("signed already"), "{e}");
}

#[test]
fn bytes_that_are_not_a_package_are_refused_by_what_is_wrong() {
    // Catches: a cut or altered file signed anyway, and a read past the end of a header.
    let bytes = package();
    let refused = |b: &[u8]| match split(b) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a bad package was accepted"),
    };
    assert!(refused(b"nothing").contains("no lead"), "short");
    assert!(
        refused(&bytes[..50]).contains("no lead"),
        "a lead cut short"
    );
    let mut lead = bytes.clone();
    lead[0] = 0;
    assert!(refused(&lead).contains("no lead"));
    assert!(refused(&bytes[..96]).contains("a header without its magic"));
    assert!(refused(&bytes[..100]).contains("a header without its magic"));
    assert!(refused(&bytes[..110]).contains("cut short"));
    let mut cut = bytes.clone();
    cut[96 + 8..96 + 12].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(refused(&cut).contains("cut short"));
    let parts = split(&bytes).expect("parts");
    let (start, _) = parts.header_range();
    assert!(refused(&bytes[..start]).contains("without its magic"));
    assert!(refused(&bytes[..start + 20]).contains("cut short"));
    let mut magic = bytes.clone();
    magic[start] = 0;
    assert!(refused(&magic).contains("without its magic"));
    let mut form = bytes.clone();
    // The first datum is moved by one byte: the header is not in the canonical form.
    form[96 + 16 + 16 + 11] += 1;
    assert!(refused(&form).contains("not in the form"));
}

/// A signature header of one entry, `kind` `count` and `data`, with its region, as bytes.
fn header_of(tag: u32, kind: u32, count: u32, data: &[u8]) -> Vec<u8> {
    let mut store = data.to_vec();
    store.extend([0, 0, 0, 62, 0, 0, 0, 7, 0xff, 0xff, 0xff, 0xe0, 0, 0, 0, 16]);
    let mut out = HEADER_MAGIC.to_vec();
    out.extend(2u32.to_be_bytes());
    out.extend((store.len() as u32).to_be_bytes());
    for word in [62, 7, data.len() as u32, 16, tag, kind, 0, count] {
        out.extend(word.to_be_bytes());
    }
    out.extend(store);
    out
}

#[test]
fn the_data_of_an_entry_is_measured_by_its_type() {
    // Catches: a datum cut off or run over by a miscount, for each type a signature header holds.
    assert_eq!(data_len(0, 0, b"").expect("null"), 0);
    assert_eq!(data_len(1, 3, b"abcd").expect("char"), 3);
    assert_eq!(data_len(3, 2, b"abcdef").expect("int16"), 4);
    assert_eq!(data_len(4, 2, &[0; 9]).expect("int32"), 8);
    assert_eq!(data_len(5, 1, &[0; 9]).expect("int64"), 8);
    assert_eq!(data_len(6, 1, b"ab\0cd").expect("string"), 3);
    assert_eq!(data_len(8, 2, b"a\0bc\0d").expect("strings"), 5);
    assert!(data_len(6, 1, b"abc").is_err());
    assert!(data_len(8, 2, b"a\0").is_err());
    assert!(data_len(4, 2, b"abcd").is_err());
    assert!(
        data_len(99, 1, b"abcd")
            .unwrap_err()
            .to_string()
            .contains("type 99")
    );
    assert_eq!(alignment(3), 2);
    assert_eq!(alignment(4), 4);
    assert_eq!(alignment(5), 8);
    assert_eq!(alignment(7), 1);
    // An entry of each type, written and read again.
    let mut entries = vec![
        Entry {
            tag: 100,
            kind: 6,
            count: 1,
            data: b"ab\0".to_vec(),
        },
        Entry {
            tag: 101,
            kind: 4,
            count: 1,
            data: vec![0, 0, 0, 7],
        },
        Entry {
            tag: 102,
            kind: 5,
            count: 1,
            data: vec![0, 0, 0, 0, 0, 0, 0, 9],
        },
        Entry {
            tag: 103,
            kind: 3,
            count: 1,
            data: vec![0, 5],
        },
    ];
    let sig = Sig {
        entries: entries.clone(),
    };
    entries.sort_by_key(|e| e.tag);
    assert_eq!(read_sig(&sig.write()).expect("read").0.entries, entries);
}

#[test]
fn an_entry_outside_its_store_is_refused() {
    // Catches: an offset past the store read as data.
    let mut bytes = header_of(100, 7, 2, b"ab");
    bytes[16 + 16 + 8..16 + 16 + 12].copy_from_slice(&1000u32.to_be_bytes());
    let e = read_sig(&bytes).unwrap_err().to_string();
    assert!(e.contains("outside the store"), "{e}");
    let cut = header_of(100, 7, 100, b"ab");
    assert!(
        read_sig(&cut)
            .unwrap_err()
            .to_string()
            .contains("cut short")
    );
}

#[test]
fn the_signer_is_read_from_the_packet_in_the_forms_gnupg_writes() {
    // Catches: a signature by another key accepted for ours, an algorithm misread, and a packet
    // read past its end.
    let sig = packet(1, FPR, b"mpi");
    assert_eq!(
        signer(&sig).expect("signer"),
        Signer {
            algorithm: 1,
            key_id: KEY_ID.to_owned()
        }
    );
    // The old format with a one-octet length, the new format with one and two octets.
    let short = packet(22, FPR, b"");
    let body_short = &short[3..];
    let mut old_one = vec![0x88, body_short.len() as u8];
    old_one.extend(body_short);
    assert_eq!(signer(&old_one).expect("old one").algorithm, 22);
    let mut new_one = vec![0xc2, body_short.len() as u8];
    new_one.extend(body_short);
    assert_eq!(signer(&new_one).expect("new one").key_id, KEY_ID);
    let long = packet(1, FPR, &[7; 200]);
    let body_long = &long[3..];
    let n = body_long.len() - 192;
    let mut new_two = vec![0xc2, (n >> 8) as u8 + 192, (n & 255) as u8];
    new_two.extend(body_long);
    assert_eq!(signer(&new_two).expect("new two").algorithm, 1);
    for (what, bytes) in [
        ("empty", Vec::new()),
        ("not a packet", vec![0x01, 0x02]),
        ("old one cut", vec![0x88]),
        ("old two cut", vec![0x89, 0x01]),
        ("new cut", vec![0xc2]),
        ("new five", vec![0xc2, 0xff, 0, 0, 0, 1, 4]),
        ("new two cut", vec![0xc2, 0xc0]),
        ("body cut", vec![0x88, 10, 4]),
    ] {
        let e = signer(&bytes).unwrap_err().to_string();
        assert!(e.contains("not an OpenPGP signature packet"), "{what}: {e}");
    }
}

#[test]
fn a_signature_that_is_not_version_4_or_names_no_issuer_is_refused() {
    // Catches: a signature whose key cannot be checked taken for ours.
    let refuse = |bytes: &[u8]| signer(bytes).unwrap_err().to_string();
    let mut v3 = packet(1, FPR, b"mpi");
    v3[3] = 3;
    assert!(refuse(&v3).contains("not a version 4"));
    let mut short = vec![0x88, 3, 4, 0, 1];
    assert!(refuse(&short).contains("not a version 4"));
    short = vec![0x88, 6, 4, 0, 1, 8, 0, 9];
    assert!(refuse(&short).contains("cut short"));
    short = vec![0x88, 8, 4, 0, 1, 8, 0, 0, 0, 5];
    assert!(refuse(&short).contains("cut short"));
    // The area of unhashed subpackets is missing altogether.
    short = vec![0x88, 6, 4, 0, 1, 8, 0, 0];
    assert!(refuse(&short).contains("cut short"));
    // No subpackets: no issuer.
    let none = vec![0x88, 10, 4, 0, 1, 8, 0, 0, 0, 0, 0, 0];
    assert!(refuse(&none).contains("names no issuer"));
    // A key id only in the unhashed area is the issuer; a subpacket of another kind is skipped.
    let mut only_id = vec![4, 0, 1, 8, 0, 3, 2, 2, 0, 0, 10, 9, 16];
    only_id.extend(&hex::decode(KEY_ID).expect("hex"));
    only_id.extend([0, 0]);
    let mut packet = vec![0x88, only_id.len() as u8];
    packet.extend(&only_id);
    assert_eq!(signer(&packet).expect("signer").key_id, KEY_ID);
    // A subpacket cut short, one of five-octet length, and one of two-octet length.
    let bad = |hashed: &[u8]| {
        let mut body = vec![4, 0, 1, 8, 0, hashed.len() as u8];
        body.extend(hashed);
        body.extend([0, 0]);
        let mut packet = vec![0x88, body.len() as u8];
        packet.extend(body);
        signer(&packet).unwrap_err().to_string()
    };
    assert!(bad(&[9, 2]).contains("not in a form"));
    assert!(bad(&[255, 0, 0, 0, 1, 2]).contains("not in a form"));
    assert!(bad(&[192]).contains("not in a form"));
    assert!(bad(&[0]).contains("not in a form"));
}

#[test]
fn the_tag_follows_the_public_key_algorithm() {
    // Catches: an RSA signature filed under the DSA tag (or the reverse), where a reader would not
    // look for it.
    assert_eq!(tag_of(1).expect("rsa"), TAG_RSA);
    for algorithm in [17, 19, 22] {
        assert_eq!(tag_of(algorithm).expect("dsa"), TAG_DSA);
    }
    assert!(tag_of(2).unwrap_err().to_string().contains("algorithm 2"));
}

#[test]
fn a_subpacket_with_a_two_octet_length_is_skipped_and_the_issuer_after_it_is_found() {
    // Catches: an issuer lost behind a long subpacket (a notation, say), which GnuPG writes and
    // whose length takes two octets.
    let fpr = hex::decode(FPR).expect("hex");
    let mut hashed = vec![192, 0, 20];
    hashed.extend([0; 191]);
    hashed.extend([22, 33, 4]);
    hashed.extend(&fpr);
    let mut body = vec![4, 0, 1, 8];
    body.extend((hashed.len() as u16).to_be_bytes());
    body.extend(&hashed);
    body.extend([0, 0, 0, 0]);
    let mut packet = vec![0x89];
    packet.extend((body.len() as u16).to_be_bytes());
    packet.extend(body);
    assert_eq!(signer(&packet).expect("signer").key_id, KEY_ID);
}
