//! The part of the declaration (`/etc/updater.d/tierstore.toml`) the hook checks its record
//! against: the two private ports, the two slot units and the record directory.
//!
//! ```toml
//! state_dir    = "/var/lib/updater/hooks/tierstore"
//! health_ports = [9301, 9302]
//!
//! [hook]
//! units = ["tierstore@9301.service", "tierstore@9302.service"]
//! ```
//!
//! Keys the hook does not read are ignored: the file is the updater's. A unit name is built
//! from a validated port by [`unit_name`], never read from the record.

use std::path::PathBuf;

use serde::Deserialize;

use super::plain::is_plain;

/// Where the declaration is installed.
pub const DECLARATION_PATH: &str = "/etc/updater.d/tierstore.toml";

/// The public port clients call; the redirect sends it to the serving slot.
pub const PUBLIC_PORT: u16 = 9300;

/// The slot unit of a private port: one template, `%i` is the port.
#[must_use]
pub fn unit_name(port: u16) -> String {
    format!("tierstore@{port}.service")
}

/// Why a declaration was refused.
#[derive(Debug, thiserror::Error)]
pub enum DeclarationError {
    /// The text is not TOML, or lacks a key the hook needs, or has one of the wrong type.
    #[error("the declaration is not usable: {0}")]
    Parse(#[from] toml::de::Error),
    /// `health_ports` is not two different non-zero ports other than the public one.
    #[error("health_ports must be two different ports, not 0 and not {PUBLIC_PORT}")]
    Ports,
    /// `hook.units` is not exactly the two slot units of the two ports.
    #[error("hook.units must be exactly the slot units of the two health ports")]
    Units,
    /// `state_dir` is not an absolute path of plain components.
    #[error("state_dir must be an absolute path of plain components")]
    NotPlain,
    /// `state_dir` is a path the hook must never use as its record directory.
    #[error("state_dir {0:?} is a reserved path")]
    Reserved(PathBuf),
    /// `uid`, the service account, is not an account name.
    #[error("uid must be an account name of letters, digits and _ . - (at most 32)")]
    Service,
}

#[derive(Deserialize)]
struct Wire {
    state_dir: PathBuf,
    health_ports: Vec<u16>,
    hook: WireHook,
    #[serde(default)]
    uid: Option<String>,
}

#[derive(Deserialize)]
struct WireHook {
    units: Vec<String>,
}

/// What the hook reads of its declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// The record directory.
    pub state_dir: PathBuf,
    /// The two private ports, in the declared order.
    pub ports: [u16; 2],
    /// The service account (`uid`), which must be a member of the state directory's group;
    /// `None` when the declaration names none.
    pub service: Option<String>,
}

impl Declaration {
    /// Reads the text of a declaration.
    ///
    /// # Errors
    /// [`DeclarationError`]: see its variants.
    pub fn parse(text: &str) -> Result<Declaration, DeclarationError> {
        let wire: Wire = toml::from_str(text)?;
        let &[first, second] = wire.health_ports.as_slice() else {
            return Err(DeclarationError::Ports);
        };
        if first == second || [first, second].iter().any(|p| *p == 0 || *p == PUBLIC_PORT) {
            return Err(DeclarationError::Ports);
        }
        let mut listed = wire.hook.units;
        listed.sort();
        let mut wanted = vec![unit_name(first), unit_name(second)];
        wanted.sort();
        if listed != wanted {
            return Err(DeclarationError::Units);
        }
        if !is_plain(&wire.state_dir) {
            return Err(DeclarationError::NotPlain);
        }
        Ok(Declaration {
            state_dir: wire.state_dir,
            ports: [first, second],
            service: wire.uid,
        })
    }

    /// Whether `port` is one of the two private ports.
    #[must_use]
    pub fn has(&self, port: u16) -> bool {
        self.ports.contains(&port)
    }

    /// The private port that is not `port`.
    #[must_use]
    pub fn other(&self, port: u16) -> u16 {
        if self.ports[0] == port {
            self.ports[1]
        } else {
            self.ports[0]
        }
    }
}
