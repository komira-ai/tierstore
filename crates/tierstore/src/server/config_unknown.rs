//! The keys of the configuration file that this release does not know.
//!
//! A release that adds an optional key writes it into files that the release before reads on a
//! downgrade, so a key the reader does not know is not a fault: it is named in one warning and
//! left out, and the node starts. A key it knows, with a value of the wrong type, is still refused
//! (by the typed read of the file), and so is a key whose name says it holds a secret (see
//! `find_secret` in `config.rs`, which looks at every key, known or not).

use std::cell::Cell;
use std::collections::BTreeSet;

use serde::de::{self, DeserializeOwned, Deserializer, Error as _, Visitor};

use super::config::{File, OciTable, ServerTable, TierTable};
use super::config_cache::LocalCacheTable;
use super::config_policy::Policy;

/// A deserializer that is asked for a struct and answers with the names of its fields, which is
/// all the derive of `Deserialize` tells a deserializer about it.
struct Probe<'a>(&'a Cell<&'static [&'static str]>);

impl<'de> Deserializer<'de> for Probe<'_> {
    type Error = de::value::Error;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.set(fields);
        Err(Self::Error::custom("only the field names were asked for"))
    }

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(Self::Error::custom("only a struct is probed"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier
        ignored_any
    }
}

/// The names of the keys that the struct `T` of a table reads, so that a key added to the struct
/// is known without anyone listing it a second time.
pub(super) fn fields_of<T: DeserializeOwned>() -> &'static [&'static str] {
    let fields = Cell::new(&[][..]);
    // The probe always ends in an error: what it found is in the cell.
    let _ = T::deserialize(Probe(&fields));
    fields.get()
}

/// The `table.key` of every key of `value` (a table, or an array of tables) that is not in `known`.
fn unknown_in(path: &str, value: &toml::Value, known: &[&str], found: &mut BTreeSet<String>) {
    let tables: Vec<&toml::Table> = match value {
        toml::Value::Array(items) => items.iter().filter_map(toml::Value::as_table).collect(),
        other => other.as_table().into_iter().collect(),
    };
    for key in tables.iter().flat_map(|table| table.keys()) {
        if !known.contains(&key.as_str()) {
            found.insert(format!("{path}.{key}"));
        }
    }
}

/// The warning for each key of `file` that this release does not know, once for each key and in
/// the order of the names. A key is named by its table and its name (`server.new_knob`), a table
/// of the file's top level by its name; a table that is not known is one key, and what is in it
/// is not looked at. The text of a warning never holds a value.
pub(super) fn unknown_key_warnings(file: &toml::Table) -> Vec<String> {
    let mut found = BTreeSet::new();
    let top = fields_of::<File>();
    for (name, value) in file {
        let known: &[&str] = match name.as_str() {
            "server" => fields_of::<ServerTable>(),
            "hot_store" | "cold_store" => fields_of::<TierTable>(),
            "oci" => fields_of::<OciTable>(),
            "local_cache" => fields_of::<LocalCacheTable>(),
            "policy" => fields_of::<Policy>(),
            _ => &[],
        };
        if top.contains(&name.as_str()) {
            unknown_in(name, value, known, &mut found);
        } else {
            found.insert(name.clone());
        }
    }
    found
        .into_iter()
        .map(|key| {
            format!("tierstore: config key {key} is not known to this release and is ignored")
        })
        .collect()
}

/// Says on stderr, once for each key, which keys of the file `text` (a file that has been read
/// without a fault) this release does not know.
pub(super) fn announce_unknown_keys(text: &str) {
    let file = toml::from_str::<toml::Table>(text).unwrap_or_default();
    for warning in unknown_key_warnings(&file) {
        eprintln!("{warning}");
    }
}

#[cfg(test)]
#[path = "config_unknown_tests.rs"]
mod tests;
