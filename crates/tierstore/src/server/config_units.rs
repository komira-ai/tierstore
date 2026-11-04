//! The values of the configuration file that are written as text: a duration (`15m`) and a size
//! of bytes (`500GiB`), as the flags of the same names have them.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::cli::parse_duration;

/// A duration in the file: a string such as `250ms` or `15m`, as the flags have it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Dur(pub Duration);

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(DurVisitor)
    }
}

struct DurVisitor;

impl Visitor<'_> for DurVisitor {
    type Value = Dur;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a duration such as \"15m\" (a number and ms, s, m or h)")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Dur, E> {
        match parse_duration(text) {
            Ok(duration) => Ok(Dur(duration)),
            // An invalid value, not a custom message and not the reason of the flag (which holds
            // the text): the refusal of a file names the key and the kind of value it takes and
            // never shows what was written (see `syntax`).
            Err(_) => Err(E::invalid_value(de::Unexpected::Str(text), &self)),
        }
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_duration(self.0))
    }
}

/// The duration as the largest of `h`, `m`, `s` and `ms` that holds it in whole numbers, such
/// that [`parse_duration`] reads the text back to the same duration.
pub(super) fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis == 0 {
        return "0s".to_owned();
    }
    let units = [(3_600_000, "h"), (60_000, "m"), (1000, "s")];
    for (size, unit) in units {
        if millis.is_multiple_of(size) {
            return format!("{}{unit}", millis / size);
        }
    }
    format!("{millis}ms")
}

/// A number of bytes in the file: an integer, or a string of a number and `B`, `KiB`, `MiB`,
/// `GiB` or `TiB`, such as `500GiB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Size(pub u64);

impl Serialize for Size {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SizeVisitor)
    }
}

struct SizeVisitor;

impl Visitor<'_> for SizeVisitor {
    type Value = Size;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number of bytes, or a string such as \"500GiB\"")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Size, E> {
        match u64::try_from(value) {
            Ok(bytes) => Ok(Size(bytes)),
            Err(_) => Err(E::custom("a size is not negative")),
        }
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Size, E> {
        parse_size(text).map(Size).map_err(E::custom)
    }
}

/// A size: a whole number and `B`, `KiB`, `MiB`, `GiB` or `TiB`.
pub(super) fn parse_size(text: &str) -> Result<u64, String> {
    let bad = || "not a size; use a number and B, KiB, MiB, GiB or TiB, as in 500GiB".to_owned();
    let unit_at = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (digits, unit) = text.split_at(unit_at);
    let count: u64 = digits.parse().map_err(|_| bad())?;
    let shift = match unit {
        "B" => 0,
        "KiB" => 10,
        "MiB" => 20,
        "GiB" => 30,
        "TiB" => 40,
        _ => return Err(bad()),
    };
    count.checked_mul(1 << shift).ok_or_else(bad)
}
