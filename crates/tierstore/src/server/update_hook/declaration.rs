//! The part of the declaration (`/etc/updater.d/tierstore.toml`) the hook checks its record
//! against: the two private ports, the two slot units, the record directory and the service
//! account.
//!
//! ```toml
//! uid          = "tierstore"
//! state_dir    = "/var/lib/updater/hooks/tierstore"
//! health_ports = [9301, 9302]
//!
//! [hook]
//! units = ["tierstore@9301.service", "tierstore@9302.service"]
//! ```
//!
//! Keys the hook does not read are ignored: the file is the updater's. A unit name is built
//! from a validated port by [`unit_name`], never read from the record.
//!
//! A macOS host runs one instance under launchd, and its declaration
//! ([`LAUNCHD_DECLARATION_PATH`]) is read by [`LaunchdDeclaration::parse`]: one health port, the
//! launchd label of the service in place of the slot units, a record directory outside the
//! macOS system trees, and the service account, which it must name:
//!
//! ```toml
//! uid          = "_tierstore"
//! state_dir    = "/var/db/updater/hooks/tierstore"
//! health_ports = [9300]
//!
//! [hook]
//! service = "com.komira.tierstore"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::machine::LAUNCHD_LABEL;
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

/// Paths that are never a record directory, whole: the root and the directories every other
/// path of the host sits under.
/// The updater's own directories, whole (what it keeps under them is its and the hooks').
const RESERVED: [&str; 5] = [
    "/",
    "/var",
    "/var/lib",
    "/var/lib/updater",
    "/var/lib/updater/hooks",
];

/// Trees that are never a record directory, nor anything under them: system and virtual
/// directories and the service's own cache and state.
const RESERVED_TREES: [&str; 20] = [
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib32",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/tmp",
    "/usr",
    "/var/cache",
    "/var/lib/tierstore",
    "/var/log",
    "/var/tmp",
];

/// Where the declaration of a macOS host is installed: `/etc` is a link to `/private/etc` there,
/// and the hook opens no directory above its declaration through a link.
pub const LAUNCHD_DECLARATION_PATH: &str = "/usr/local/etc/updater.d/tierstore.toml";

/// The paths of a macOS host that are never a record directory, whole: the root, the
/// directories every other path sits under, and the updater's own directories.
const LAUNCHD_RESERVED: [&str; 7] = [
    "/",
    "/usr",
    "/usr/local",
    "/var",
    "/var/db",
    "/var/db/updater",
    "/var/db/updater/hooks",
];

/// The trees of a macOS host that are never a record directory, nor anything under them: the
/// system's own, the users', the links into `/private` (and `/private` itself, so that one
/// directory has one spelling), the programs and settings under `/usr/local`, and the service's
/// releases, links, cache and logs (`/usr/local/tierstore`).
const LAUNCHD_RESERVED_TREES: [&str; 30] = [
    "/Applications",
    "/Library",
    "/System",
    "/Users",
    "/Volumes",
    "/bin",
    "/cores",
    "/dev",
    "/etc",
    "/opt",
    "/private",
    "/sbin",
    "/tmp",
    "/usr/bin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/sbin",
    "/usr/share",
    "/usr/local/bin",
    "/usr/local/etc",
    "/usr/local/kbf",
    "/usr/local/lib",
    "/usr/local/libexec",
    "/usr/local/tierstore",
    "/var/folders",
    "/var/log",
    "/var/root",
    "/var/run",
    "/var/tmp",
    "/var/vm",
];

/// Whether every component of `path` (already plain) is one a service manager reads as a path
/// and not as a prefix (`-`, `+`, `!`), and nothing in it is whitespace or a control character.
fn is_clean(path: &Path) -> bool {
    let text = path.to_string_lossy();
    !text.chars().any(|c| c.is_whitespace() || c.is_control())
        && text
            .split('/')
            .all(|part| !part.starts_with(['-', '+', '!']))
}

/// Whether `path` (already plain) is one the hook must not use as its record directory.
fn is_reserved(path: &Path) -> bool {
    RESERVED.iter().any(|reserved| path == Path::new(reserved))
        || RESERVED_TREES
            .iter()
            .any(|tree| path.starts_with(Path::new(tree)))
}

/// Whether `path` (already plain) is one the hook of a macOS host must not use as its record
/// directory.
fn is_launchd_reserved(path: &Path) -> bool {
    LAUNCHD_RESERVED
        .iter()
        .any(|reserved| path == Path::new(reserved))
        || LAUNCHD_RESERVED_TREES
            .iter()
            .any(|tree| path.starts_with(Path::new(tree)))
}

/// Whether `name` is an account name: 1 to 32 letters, digits, `_`, `.` and `-`, not starting
/// with `-`.
fn is_account(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
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
    #[error(
        "state_dir must be an absolute path of plain components: no whitespace, and no component starting with - + or !"
    )]
    NotPlain,
    /// `state_dir` is a path the hook must never use as its record directory.
    #[error("state_dir {0:?} is a reserved path")]
    Reserved(PathBuf),
    /// `uid`, the service account, is not an account name.
    #[error("uid must be an account name of letters, digits and _ . - (at most 32)")]
    Service,
    /// A macOS declaration: `health_ports` is not one non-zero port.
    #[error("health_ports must be one port, not 0, on a host that runs one instance")]
    OnePort,
    /// A macOS declaration: `hook.service` is not the launchd label of the service.
    #[error("hook.service must be the launchd label {LAUNCHD_LABEL}")]
    Label,
    /// A macOS declaration lists slot units: there are none under launchd.
    #[error("hook.units has no place on a host that runs one instance under launchd")]
    SlotUnits,
    /// A macOS declaration names no service account.
    #[error("uid, the service account, is required on a host that runs one instance")]
    NoService,
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

#[derive(Deserialize)]
struct LaunchdWire {
    state_dir: PathBuf,
    health_ports: Vec<u16>,
    hook: LaunchdWireHook,
    #[serde(default)]
    uid: Option<String>,
}

#[derive(Deserialize)]
struct LaunchdWireHook {
    service: String,
    #[serde(default)]
    units: Option<toml::Value>,
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
        if !is_plain(&wire.state_dir) || !is_clean(&wire.state_dir) {
            return Err(DeclarationError::NotPlain);
        }
        if is_reserved(&wire.state_dir) {
            return Err(DeclarationError::Reserved(wire.state_dir));
        }
        if wire.uid.as_deref().is_some_and(|name| !is_account(name)) {
            return Err(DeclarationError::Service);
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

/// What the hook of a macOS host reads of its declaration: one instance, under launchd.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchdDeclaration {
    /// The record directory.
    pub state_dir: PathBuf,
    /// The port the one instance serves on, on loopback.
    pub port: u16,
    /// The service account (`uid`) the launchd job runs as.
    pub service: String,
}

impl LaunchdDeclaration {
    /// Reads the text of a macOS declaration. The rules on `state_dir` are those of
    /// [`Declaration::parse`] with the reserved paths of macOS: absolute, plain, no whitespace,
    /// no component a service manager reads as a prefix, and outside the system trees.
    ///
    /// # Errors
    /// [`DeclarationError`]: see its variants.
    pub fn parse(text: &str) -> Result<LaunchdDeclaration, DeclarationError> {
        let wire: LaunchdWire = toml::from_str(text)?;
        let &[port] = wire.health_ports.as_slice() else {
            return Err(DeclarationError::OnePort);
        };
        if port == 0 {
            return Err(DeclarationError::OnePort);
        }
        if wire.hook.units.is_some() {
            return Err(DeclarationError::SlotUnits);
        }
        if wire.hook.service != LAUNCHD_LABEL {
            return Err(DeclarationError::Label);
        }
        if !is_plain(&wire.state_dir) || !is_clean(&wire.state_dir) {
            return Err(DeclarationError::NotPlain);
        }
        if is_launchd_reserved(&wire.state_dir) {
            return Err(DeclarationError::Reserved(wire.state_dir));
        }
        let service = wire.uid.ok_or(DeclarationError::NoService)?;
        if !is_account(&service) {
            return Err(DeclarationError::Service);
        }
        Ok(LaunchdDeclaration {
            state_dir: wire.state_dir,
            port,
            service,
        })
    }
}
