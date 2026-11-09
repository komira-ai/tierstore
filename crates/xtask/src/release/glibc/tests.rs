use super::*;
use crate::testdir::TestDir;

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-glibc-unit"), name)
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The 64-byte header of a 64-bit little-endian ELF file with `sections` section headers at
/// byte 64.
fn header(sections: u16) -> Vec<u8> {
    let mut h = b"\x7fELF\x02\x01\x01".to_vec();
    h.resize(0x28, 0);
    put_u64(&mut h, 64); // e_shoff
    h.resize(0x3a, 0);
    put_u16(&mut h, 64); // e_shentsize
    put_u16(&mut h, sections); // e_shnum
    h.resize(64, 0);
    h
}

/// One section header.
fn section(kind: u32, offset: u64, size: u64, link: u32, info: u32) -> Vec<u8> {
    let mut s = Vec::new();
    put_u32(&mut s, 0); // sh_name
    put_u32(&mut s, kind);
    put_u64(&mut s, 0); // sh_flags
    put_u64(&mut s, 0); // sh_addr
    put_u64(&mut s, offset);
    put_u64(&mut s, size);
    put_u32(&mut s, link);
    put_u32(&mut s, info);
    put_u64(&mut s, 8); // sh_addralign
    put_u64(&mut s, 0); // sh_entsize
    s
}

/// An ELF file that needs, from each library of `needs`, the symbol versions listed: the layout
/// is the header, three section headers (null, `.dynstr`, `.gnu.version_r`), the version
/// requirements and the string table last, so a file cut anywhere is cut in a table a reader
/// uses.
fn elf(needs: &[(&str, &[&str])]) -> Vec<u8> {
    let mut strings = vec![0u8];
    let mut name = |text: &str| {
        let at = strings.len() as u32;
        strings.extend_from_slice(text.as_bytes());
        strings.push(0);
        at
    };
    let mut need = Vec::new();
    for (i, (file, versions)) in needs.iter().enumerate() {
        let file_name = name(file);
        put_u16(&mut need, 1); // vn_version
        put_u16(&mut need, versions.len() as u16);
        put_u32(&mut need, file_name);
        put_u32(&mut need, 16); // vn_aux
        let last = i + 1 == needs.len();
        put_u32(
            &mut need,
            if last {
                0
            } else {
                16 + 16 * versions.len() as u32
            },
        );
        for (j, version) in versions.iter().enumerate() {
            put_u32(&mut need, 0); // vna_hash
            put_u16(&mut need, 0); // vna_flags
            put_u16(&mut need, 2); // vna_other
            put_u32(&mut need, name(version));
            put_u32(&mut need, if j + 1 == versions.len() { 0 } else { 16 });
        }
    }
    let need_at = 64 + 3 * 64;
    let strings_at = need_at + need.len() as u64;
    let mut file = header(3);
    file.extend(section(0, 0, 0, 0, 0));
    file.extend(section(3, strings_at, strings.len() as u64, 0, 0));
    file.extend(section(
        SHT_GNU_VERNEED,
        need_at,
        need.len() as u64,
        1,
        needs.len() as u32,
    ));
    file.extend(need);
    file.extend(strings);
    file
}

fn put(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("tierstore");
    std::fs::write(&path, bytes).expect("write the binary");
    path
}

#[test]
fn a_binary_needing_glibc_2_39_is_refused_under_2_28() {
    // Catches: the failure of the first release's install checks, a binary built on Ubuntu
    // 24.04 that asks for GLIBC_2.39 and cannot start on AlmaLinux 9 or Amazon Linux 2023.
    let dir = scratch("refused");
    let path = put(&dir, &elf(&[("libc.so.6", &["GLIBC_2.2.5", "GLIBC_2.39"])]));
    let error = check_glibc(&path, FLOOR).expect_err("a binary above the floor");
    assert_eq!(
        error,
        Error::Failed(format!(
            "{} needs GLIBC_2.39 (it is linked against a newer glibc than 2.28, and fails to start on a host with an older one)",
            path.display()
        ))
    );
}

#[test]
fn every_version_above_the_floor_is_named_once_in_order() {
    // Catches: a report that stops at the first version, lists one twice, or sorts 2.4 after
    // 2.39 as text does; and a limit compared as text (2.4 is below 2.28).
    let dir = scratch("named");
    let path = put(
        &dir,
        &elf(&[
            ("libc.so.6", &["GLIBC_2.39", "GLIBC_2.4", "GLIBC_2.29"]),
            ("libm.so.6", &["GLIBC_2.29", "GLIBC_2.28"]),
        ]),
    );
    let Err(Error::Failed(said)) = check_glibc(&path, "2.28") else {
        panic!("a binary above the floor");
    };
    assert!(said.contains("needs GLIBC_2.29, GLIBC_2.39 ("), "{said}");
}

#[test]
fn a_binary_needing_no_more_than_2_28_passes_and_says_its_highest_version() {
    // Catches: a check that refuses a binary at exactly the floor, compares the versions as
    // text (2.4 and 2.2.5 sort above 2.28 as text), or reports the last version listed instead
    // of the highest.
    let dir = scratch("passes");
    let path = put(
        &dir,
        &elf(&[
            ("libc.so.6", &["GLIBC_2.28", "GLIBC_2.2.5", "GLIBC_2.4"]),
            ("libm.so.6", &["GLIBC_2.2.5"]),
        ]),
    );
    assert_eq!(
        check_glibc(&path, FLOOR),
        Ok(vec![format!(
            "release check-glibc: {} needs GLIBC_2.28 at most; the limit is GLIBC_2.28",
            path.display()
        )])
    );
    assert!(check_glibc(&path, "2.27").is_err());
    assert!(check_glibc(&path, "2.28").is_ok());
}

#[test]
fn versions_that_are_not_glibc_numbers_are_not_counted() {
    // Catches: GLIBC_PRIVATE or another library's version (libgcc_s's GCC_3.0) read as a glibc
    // version, which would refuse a binary on a name.
    let dir = scratch("other");
    let path = put(
        &dir,
        &elf(&[
            ("libc.so.6", &["GLIBC_PRIVATE", "GLIBC_2.17"]),
            ("libgcc_s.so.1", &["GCC_3.0"]),
        ]),
    );
    let said = check_glibc(&path, FLOOR).expect("below the floor");
    assert!(said[0].contains("needs GLIBC_2.17 at most"), "{said:?}");
}

#[test]
fn a_binary_with_no_version_requirements_needs_no_glibc() {
    // Catches: a static binary (no `.gnu.version_r`) refused for having nothing to list.
    let dir = scratch("static");
    let path = put(&dir, &header(0));
    assert_eq!(
        check_glibc(&path, FLOOR),
        Ok(vec![format!(
            "release check-glibc: {} needs no glibc symbol version; the limit is GLIBC_2.28",
            path.display()
        )])
    );
}

#[test]
fn a_file_that_is_not_a_64_bit_little_endian_elf_is_refused() {
    // Catches: a shell script, a 32-bit or big-endian binary read as if it were 64-bit
    // little-endian (garbage offsets, or a pass with nothing checked).
    let mut thirty_two = header(0);
    thirty_two[4] = 1;
    let mut big = header(0);
    big[5] = 2;
    for (name, bytes, why) in [
        ("script", b"#!/bin/sh\n".to_vec(), "not an ELF file"),
        ("empty", Vec::new(), "not an ELF file"),
        ("32", thirty_two, "not a 64-bit little-endian ELF file"),
        ("big", big, "not a 64-bit little-endian ELF file"),
    ] {
        let dir = scratch(&format!("not-elf-{name}"));
        let path = put(&dir, &bytes);
        assert_eq!(
            check_glibc(&path, FLOOR),
            Err(Error::Failed(format!("{}: {why}", path.display()))),
            "{name}"
        );
    }
}

#[test]
fn a_file_cut_short_or_with_tables_outside_it_is_refused_not_read_past() {
    // Catches: a panic or a pass on a truncated download: every proper prefix of the file is
    // refused (each table is read through the same bounds check), and so are a section table
    // and a string table whose offsets are out of range.
    let full = elf(&[("libc.so.6", &["GLIBC_2.2.5", "GLIBC_2.39"])]);
    for len in 0..full.len() {
        assert!(
            glibc_versions(&full[..len]).is_err(),
            "prefix of {len} bytes"
        );
    }
    assert!(glibc_versions(&full).is_ok());
    let mut far = full.clone();
    far[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(
        glibc_versions(&far).is_err(),
        "section table at the end of the address space"
    );
    let mut no_strings = full.clone();
    no_strings[64 + 2 * 64 + 40..64 + 2 * 64 + 44].copy_from_slice(&99u32.to_le_bytes());
    assert!(
        glibc_versions(&no_strings).is_err(),
        "string table section 99 of 3"
    );
}

#[test]
fn the_limit_is_major_dot_minor_and_the_file_must_be_there() {
    // Catches: a limit that is not read as a version (2.28.1, 2, text) and so compares as
    // something else, and a missing file reported as a pass.
    let dir = scratch("flags");
    let path = put(&dir, &header(0));
    for bad in ["2", "2.28.1", "two.28", "", "2."] {
        assert_eq!(
            check_glibc(&path, bad),
            Err(Error::Usage(format!(
                "--max {bad:?} is not <major>.<minor>"
            ))),
            "{bad}"
        );
    }
    let error = check_glibc(&dir.join("absent"), FLOOR).expect_err("no such file");
    assert!(matches!(error, Error::Spawn { .. }), "{error:?}");
}
