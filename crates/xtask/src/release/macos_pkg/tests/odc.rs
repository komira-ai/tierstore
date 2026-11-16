//! The reader of the portable cpio archive a `Payload` holds: what it reads, and each way an
//! archive can be cut or malformed.

use super::*;

#[test]
fn the_reader_gives_each_entry_its_name_mode_owner_and_bytes_up_to_the_trailer() {
    // Catches: a field read from the wrong offset, an entry's data lost, or the padding after the
    // trailer read as an entry.
    let archive = odc(&[
        odc_entry("./a", 0o100_640, 501, 20, b"abc"),
        dir_entry("./d"),
    ]);
    assert_eq!(
        read_odc(&archive),
        Ok(vec![
            CpioEntry {
                name: "./a".to_owned(),
                mode: 0o100_640,
                uid: 501,
                gid: 20,
                data: b"abc".to_vec()
            },
            CpioEntry {
                name: "./d".to_owned(),
                mode: 0o040_755,
                uid: 0,
                gid: 0,
                data: Vec::new()
            },
        ])
    );
}

#[test]
fn a_cut_or_malformed_archive_is_refused_with_the_byte_it_failed_at() {
    // Catches: a reader that panics on, or accepts, an archive that is short, has another magic,
    // a header field that is not octal, a name or data that run past the end, a name that is not
    // NUL-terminated or UTF-8, or no trailer.
    let good = odc(&[odc_entry("./a", 0o100_644, 0, 0, b"abc")]);
    assert_eq!(
        read_odc(b"0707"),
        Err("the archive ends at byte 4 without its trailer".to_owned())
    );
    assert_eq!(
        read_odc(&good[..good.len() - 120 - 100]),
        Err(format!(
            "the archive ends at byte {} without its trailer",
            good.len() - 220
        ))
    );
    let mut magic = good.clone();
    magic[..6].copy_from_slice(b"070701");
    assert_eq!(
        read_odc(&magic),
        Err("the header at byte 0 starts \"070701\", not the odc magic 070707".to_owned())
    );
    let mut nonoctal = good.clone();
    nonoctal[18] = b'9';
    assert_eq!(
        read_odc(&nonoctal),
        Err("the header field at byte 18 is not 6 octal digits".to_owned())
    );
    let mut utf8 = good.clone();
    utf8[18] = 0xff;
    assert_eq!(
        read_odc(&utf8),
        Err("the header field at byte 18 is not 6 octal digits".to_owned())
    );
    // A name size that runs past the end of the archive.
    let mut long_name = good.clone();
    long_name[59..65].copy_from_slice(b"777777");
    assert_eq!(
        read_odc(&long_name),
        Err("the archive ends inside the name that starts at byte 76".to_owned())
    );
    // Data that runs past the end of the archive.
    let mut long_data = good.clone();
    long_data[65..76].copy_from_slice(b"77777777777");
    assert_eq!(
        read_odc(&long_data),
        Err("the archive ends inside the data that starts at byte 80".to_owned())
    );
    // A name without its NUL, and a name that is not UTF-8.
    let mut no_nul = good.clone();
    no_nul[79] = b'x';
    assert_eq!(
        read_odc(&no_nul),
        Err("the name at byte 76 does not end in NUL".to_owned())
    );
    let mut bad_name = good;
    bad_name[77] = 0xff;
    assert_eq!(
        read_odc(&bad_name),
        Err("the name at byte 76 is not UTF-8".to_owned())
    );
}

#[test]
fn an_entry_name_is_a_payload_path_only_in_the_forms_pkgbuild_writes() {
    // Catches: a name without the leading ./ matching an entry of the payload.
    assert_eq!(payload_path("."), "");
    assert_eq!(payload_path("./Library"), "Library");
    assert_eq!(payload_path("Library"), "Library");
    assert_eq!(payload_path("/Library"), "/Library");
}
