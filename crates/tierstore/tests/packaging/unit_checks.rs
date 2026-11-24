//! Checks of a systemd unit's text that the tests of the templates share: the values of a key,
//! the options that give a service a mount namespace of its own, and the capabilities a unit
//! grants. Each check returns what it found, so that a test can run it on a unit and on a copy with
//! a defect.

use std::collections::BTreeSet;

/// The values of the `key=` lines of a unit, its `\` continuations joined.
pub(crate) fn values(unit: &str, key: &str) -> Vec<String> {
    let joined = unit.replace("\\\n", " ");
    let prefix = format!("{key}=");
    joined
        .lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_owned)
        .collect()
}

/// The options of systemd.exec(5) that give a service a mount namespace of its own, from which a
/// mount the process makes does not reach the host: the options of docs/design/one-service.md
/// section 2.2 (the unit's table and its path options), the older names of the path options, and
/// the options that set up a namespace to mount an image, a directory or `/proc` in, or imply
/// one (`DynamicUser=` implies `ProtectSystem=strict` and `PrivateTmp=`).
pub(crate) const MOUNT_NAMESPACE_OPTIONS: [&str; 31] = [
    "PrivateDevices",
    "ProtectSystem",
    "ProtectHome",
    "PrivateTmp",
    "ProtectKernelTunables",
    "ProtectKernelModules",
    "ProtectKernelLogs",
    "ProtectControlGroups",
    "PrivateMounts",
    "ReadWritePaths",
    "ReadOnlyPaths",
    "InaccessiblePaths",
    "BindPaths",
    "BindReadOnlyPaths",
    "TemporaryFileSystem",
    "RootDirectory",
    "PrivateUsers",
    "ProtectProc",
    "ReadWriteDirectories",
    "ReadOnlyDirectories",
    "InaccessibleDirectories",
    "ExecPaths",
    "NoExecPaths",
    "RootImage",
    "MountImages",
    "ExtensionImages",
    "ExtensionDirectories",
    "ProcSubset",
    "MountFlags",
    "MountAPIVFS",
    "DynamicUser",
];

/// The options of `MOUNT_NAMESPACE_OPTIONS` that `unit` sets, whatever their value, in the order
/// they are found: an empty list for a unit whose mount reaches the host.
pub(crate) fn mount_namespace_options(unit: &str) -> Vec<String> {
    unit.lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with(';'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, _)| key.trim())
        .filter(|key| MOUNT_NAMESPACE_OPTIONS.contains(key))
        .map(str::to_owned)
        .collect()
}

/// `unit` with `line` added as the first line of its `[Service]` section.
pub(crate) fn with_service_line(unit: &str, line: &str) -> String {
    let added = unit.replacen("[Service]\n", &format!("[Service]\n{line}\n"), 1);
    assert_ne!(added, unit, "the unit has a [Service] section");
    added
}

/// The capabilities the lines `key=` of `unit` leave the service, as systemd.exec(5) reads them:
/// each line adds its capabilities to those of the lines before it, and an empty line resets them
/// to none. A line that starts with `~` (every capability but those) or names a capability that is
/// not one word of capital letters is an error that names it.
fn capability_set(unit: &str, key: &str) -> Result<BTreeSet<String>, String> {
    let mut set = BTreeSet::new();
    for line in values(unit, key) {
        if line.trim().is_empty() {
            set.clear();
            continue;
        }
        if line.trim_start().starts_with('~') {
            return Err(format!("{key}={line} keeps every capability but those"));
        }
        for word in line.split_whitespace() {
            let well_formed = word.starts_with("CAP_")
                && word
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !well_formed {
                return Err(format!("{key}: {word} is not a capability"));
            }
            set.insert(word.to_owned());
        }
    }
    Ok(set)
}

/// Whether `unit` grants its service `CAP_SYS_ADMIN` and no other capability: the bounding set
/// and the ambient set are each `CAP_SYS_ADMIN` alone, the service runs as an account that is not
/// root, gains no privilege by what it executes, and sets no capability by another key. The error
/// says what is wrong.
pub(crate) fn grants_cap_sys_admin_alone(unit: &str) -> Result<(), String> {
    let wanted = BTreeSet::from(["CAP_SYS_ADMIN".to_owned()]);
    for key in ["CapabilityBoundingSet", "AmbientCapabilities"] {
        let set = capability_set(unit, key)?;
        if set != wanted {
            return Err(format!("{key} is {set:?}"));
        }
    }
    if values(unit, "User") != ["tierstore"] {
        return Err(format!("User is {:?}", values(unit, "User")));
    }
    if values(unit, "NoNewPrivileges") != ["yes"] {
        return Err("NoNewPrivileges is not yes".to_owned());
    }
    for key in ["SecureBits", "PermissionsStartOnly"] {
        if !values(unit, key).is_empty() {
            return Err(format!("{key} is set"));
        }
    }
    let joined = unit.replace("\\\n", " ");
    if let Some(line) = joined.lines().find(|line| raises_privileges(line)) {
        return Err(format!("{line} runs with raised privileges"));
    }
    Ok(())
}

/// Whether `line` is an `Exec*=` command whose prefix (systemd.service(5): any of `@`, `-`, `:`,
/// `+`, `!`, `!!`, in any order, after the `=` and any blanks) has `+` or `!`, which run the
/// command with privileges the unit's user and capabilities do not give it. A `+` or `!` after the
/// program, in an argument, is not a prefix.
fn raises_privileges(line: &str) -> bool {
    let Some((key, command)) = line.split_once('=') else {
        return false;
    };
    let prefix: String = command
        .trim_start()
        .chars()
        .take_while(|c| matches!(c, '@' | '-' | ':' | '+' | '!'))
        .collect();
    key.trim().starts_with("Exec") && (prefix.contains('+') || prefix.contains('!'))
}
