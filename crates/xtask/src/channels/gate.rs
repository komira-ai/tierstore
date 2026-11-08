//! `channels gate`: whether a channel job has the settings it needs (design section 5.1).
//!
//! | settings of the job | `enabled` | the job |
//! |---|---|---|
//! | none set | `false` | green, a notice names the job and the settings, nothing is written |
//! | all set | `true` | runs |
//! | some set | `false` | red, naming the missing ones |
//!
//! The third row is a half-made setting, a typo in a variable name among them: it must not leave
//! a channel silently off. A setting is set when its value is not empty; the value itself is never
//! printed, so a workflow passes `NAME=${VAR:+set}` for a secret.

use std::path::Path;

use crate::Error;
use crate::release::append_output;

/// The `NAME=VALUE` of one `--setting`: the name and whether the value is empty.
fn parse(setting: &str) -> Result<(&str, bool), Error> {
    let Some((name, value)) = setting.split_once('=') else {
        return Err(Error::Usage(format!(
            "--setting {setting:?} is not NAME=VALUE"
        )));
    };
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    if !name_ok {
        return Err(Error::Usage(format!(
            "--setting name {name:?} is not upper case letters, digits and `_`"
        )));
    }
    Ok((name, !value.is_empty()))
}

/// The `gate` verb: the lines to print.
///
/// # Errors
/// A label or setting that is not of its form, a name given twice, or some but not all of the
/// settings set.
pub fn gate(label: &str, settings: &[String], output: Option<&Path>) -> Result<Vec<String>, Error> {
    if label.is_empty() || label.chars().any(char::is_control) || label.contains("::") {
        return Err(Error::Usage(format!(
            "--label {label:?} is not a plain name"
        )));
    }
    let mut set = Vec::new();
    let mut unset = Vec::new();
    for setting in settings {
        let (name, present) = parse(setting)?;
        if set.contains(&name) || unset.contains(&name) {
            return Err(Error::Usage(format!("--setting {name} is given twice")));
        }
        if present {
            set.push(name);
        } else {
            unset.push(name);
        }
    }
    let enabled = unset.is_empty();
    if let Some(output) = output {
        append_output(output, &[format!("enabled={enabled}")])?;
    }
    if enabled {
        return Ok(vec![format!("{label}: all its settings are set")]);
    }
    if set.is_empty() {
        return Ok(vec![format!(
            "::notice title=The {label} job is off::None of its settings ({}) is set, so it writes nothing and ends green.",
            unset.join(", ")
        )]);
    }
    Err(Error::Failed(format!(
        "{label}: {} set but {} not: set all of them or none, so that a channel is never left half on",
        set.join(", "),
        unset.join(", ")
    )))
}

#[cfg(test)]
mod tests;
