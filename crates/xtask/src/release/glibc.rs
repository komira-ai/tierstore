//! `release check-glibc <binary> --max <major>.<minor>`: a Linux binary needs no glibc symbol
//! version above `--max`.
//!
//! The Linux release binaries are linked by `cargo zigbuild` against glibc 2.28 ([`FLOOR`]), so
//! they run on every distribution from RHEL 8 and Debian 10 on. A binary linked against the
//! build runner's glibc instead (2.39 on Ubuntu 24.04) fails to start on an older host with
//! "version `GLIBC_2.39' not found". This verb reads the binary's `.gnu.version_r` section (the
//! versions of other libraries it needs, `SHT_GNU_verneed`) and lists every `GLIBC_<x>.<y>[.<z>]`
//! it asks for, the way `objdump -T` or `readelf -V` shows them; it refuses the binary when one
//! is above `--max`. It reads the file and runs nothing, so any platform's binary is checked on
//! any runner. Only a 64-bit little-endian ELF file is read (x86_64 and aarch64); anything else
//! is refused. A binary with no such section (a static one) needs no glibc and passes.
//! `GLIBC_PRIVATE` is not a version number and is not counted.

use std::path::Path;

use crate::Error;

/// The glibc the Linux release binaries are linked against, and the packages require.
pub const FLOOR: &str = "2.28";

/// `SHT_GNU_verneed`, the type of `.gnu.version_r`.
const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;

/// Where `n` bytes at `base + delta` start in `bytes`, or why they are not all there.
fn start(bytes: &[u8], base: u64, delta: u64, n: usize) -> Result<usize, String> {
    base.checked_add(delta)
        .and_then(|offset| usize::try_from(offset).ok())
        .filter(|start| start.checked_add(n).is_some_and(|end| end <= bytes.len()))
        .ok_or_else(|| format!("a table points outside the file (at {base}+{delta})"))
}

fn u16_at(bytes: &[u8], base: u64, delta: u64) -> Result<u64, String> {
    let at = start(bytes, base, delta, 2)?;
    Ok(u64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]])))
}

fn u32_at(bytes: &[u8], base: u64, delta: u64) -> Result<u64, String> {
    let at = start(bytes, base, delta, 4)?;
    let mut b = [0; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    Ok(u64::from(u32::from_le_bytes(b)))
}

fn u64_at(bytes: &[u8], base: u64, delta: u64) -> Result<u64, String> {
    let at = start(bytes, base, delta, 8)?;
    let mut b = [0; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    Ok(u64::from_le_bytes(b))
}

/// The NUL-terminated text at `base + delta`.
fn text_at(bytes: &[u8], base: u64, delta: u64) -> Result<String, String> {
    let rest = &bytes[start(bytes, base, delta, 1)?..];
    let end = rest
        .iter()
        .position(|b| *b == 0)
        .ok_or("a name is not terminated")?;
    Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// `2.28` or `2.2.5` as numbers; `None` for text that is not a version.
fn numbers(text: &str) -> Option<Vec<u32>> {
    text.split('.').map(|n| n.parse().ok()).collect()
}

/// Every `GLIBC_<version>` the ELF file `elf` needs, in the order the file lists them.
///
/// # Errors
/// The file is not a 64-bit little-endian ELF file, or a table in it points outside it.
pub fn glibc_versions(elf: &[u8]) -> Result<Vec<String>, String> {
    if elf.get(..4) != Some(b"\x7fELF") {
        return Err("not an ELF file".to_owned());
    }
    if elf.get(4..6) != Some(&[2, 1]) {
        return Err("not a 64-bit little-endian ELF file".to_owned());
    }
    let (table, entry_size, count) = (
        u64_at(elf, 0x28, 0)?,
        u16_at(elf, 0x3a, 0)?,
        u16_at(elf, 0x3c, 0)?,
    );
    let mut found = Vec::new();
    for index in 0..count {
        let section = table.saturating_add(index.saturating_mul(entry_size));
        if u32_at(elf, section, 4)? != u64::from(SHT_GNU_VERNEED) {
            continue;
        }
        let (first, entries) = (u64_at(elf, section, 0x18)?, u32_at(elf, section, 0x2c)?);
        let linked = table.saturating_add(u32_at(elf, section, 0x28)?.saturating_mul(entry_size));
        let strings = u64_at(elf, linked, 0x18)?;
        let mut file = first;
        for _ in 0..entries {
            let aux_count = u16_at(elf, file, 2)?;
            let mut version = file.saturating_add(u32_at(elf, file, 8)?);
            for _ in 0..aux_count {
                let name = text_at(elf, strings, u32_at(elf, version, 8)?)?;
                if name.strip_prefix("GLIBC_").and_then(numbers).is_some() {
                    found.push(name);
                }
                version = version.saturating_add(u32_at(elf, version, 12)?);
            }
            file = file.saturating_add(u32_at(elf, file, 12)?);
        }
    }
    Ok(found)
}

/// The `release check-glibc` verb: the lines to print.
///
/// # Errors
/// `max` is not `<major>.<minor>`, the file cannot be read or is not a 64-bit little-endian ELF
/// file, or it needs a glibc version above `max` (every one is named).
pub fn check_glibc(binary: &Path, max: &str) -> Result<Vec<String>, Error> {
    let limit = numbers(max)
        .filter(|n| n.len() == 2)
        .ok_or_else(|| Error::Usage(format!("--max {max:?} is not <major>.<minor>")))?;
    let elf = std::fs::read(binary).map_err(Error::spawn(format!("read {}", binary.display())))?;
    let versions = glibc_versions(&elf)
        .map_err(|why| Error::Failed(format!("{}: {why}", binary.display())))?;
    let mut above: Vec<&String> = versions
        .iter()
        .filter(|v| numbers(&v["GLIBC_".len()..]).is_some_and(|n| n > limit))
        .collect();
    above.sort_by_key(|v| numbers(&v["GLIBC_".len()..]));
    above.dedup();
    if !above.is_empty() {
        return Err(Error::Failed(format!(
            "{} needs {} (it is linked against a newer glibc than {max}, and fails to start on a host with an older one)",
            binary.display(),
            above
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let highest = versions
        .iter()
        .max_by_key(|v| numbers(&v["GLIBC_".len()..]))
        .map_or_else(
            || "no glibc symbol version".to_owned(),
            |v| format!("{v} at most"),
        );
    Ok(vec![format!(
        "release check-glibc: {} needs {highest}; the limit is GLIBC_{max}",
        binary.display()
    )])
}

#[cfg(test)]
mod tests;
