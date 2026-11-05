//! The params file.

use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;

use super::params::{MAX_LABEL_LEN, MAX_VERSION_LEN, is_label, is_version};
use super::{
    CONFIG_FILE_KEY, LAUNCHD, LAUNCHD_LABEL_KEY, MODE_KEY, PREVIOUS_RELEASE_KEY,
    PREVIOUS_VERSION_KEY, PROMOTION_CEILING_KEY, Params, ParamsError, Previous, RESTART_MODE,
    SERVICE_MANAGER_KEY, STAGED_RELEASE_KEY, SYSTEMD, ServiceManager, TARGET_VERSION_KEY,
};

const GOOD: &str = "\"updater.staged_release\" = \"/var/lib/hostkeeper/packages/tierstore/0.5.0\"\n\
\"updater.target_version\" = \"0.5.0+0123456789ab\"\n";

const PREVIOUS: &str = "\"updater.previous_release\" = \"/var/lib/hostkeeper/packages/tierstore/0.4.0\"\n\
\"updater.previous_version\" = \"0.4.0+abcdef012345\"\n";

fn with(extra: &str) -> String {
    format!("{GOOD}{extra}")
}

fn refused(text: &str) -> ParamsError {
    Params::parse(text).unwrap_err()
}

/// Asserts that the keys the contract names are spelt as hostkeeper spells them. It would catch
/// a key renamed in the hook that hostkeeper still writes under the old name.
#[test]
fn the_keys_are_spelt_as_hostkeeper_writes_them() {
    assert_eq!(STAGED_RELEASE_KEY, "updater.staged_release");
    assert_eq!(TARGET_VERSION_KEY, "updater.target_version");
    assert_eq!(PREVIOUS_RELEASE_KEY, "updater.previous_release");
    assert_eq!(PREVIOUS_VERSION_KEY, "updater.previous_version");
    assert_eq!(PROMOTION_CEILING_KEY, "updater.promotion_ceiling_secs");
    assert_eq!(SERVICE_MANAGER_KEY, "service_manager");
    assert_eq!(LAUNCHD_LABEL_KEY, "launchd_label");
    assert_eq!(CONFIG_FILE_KEY, "config_file");
    assert_eq!((MODE_KEY, RESTART_MODE), ("mode", "restart"));
    assert_eq!((SYSTEMD, LAUNCHD), ("systemd", "launchd"));
}

/// Asserts that the two required keys are read and the rest default: a first install, systemd, no
/// ceiling, no config file, no declared restart. A key the hook does not know, of any scalar
/// type, is ignored. It would catch a hook that refuses a params file because hostkeeper added
/// a reserved key or a manifest a key of its own.
#[test]
fn the_required_keys_are_read_and_the_rest_default() {
    let params = Params::parse(&with(
        "\"updater.something_new\" = true\nother = 1.5\nname = \"x\"\ncount = 3\n",
    ))
    .unwrap();
    assert_eq!(
        params,
        Params {
            staged_release: PathBuf::from("/var/lib/hostkeeper/packages/tierstore/0.5.0"),
            target_version: "0.5.0+0123456789ab".to_owned(),
            previous: None,
            promotion_ceiling_secs: None,
            service_manager: ServiceManager::Systemd,
            config_file: None,
            declared_restart: false,
        }
    );
    assert!(params.first_install());
}

/// Asserts that a previous release and its version make an update, not a first install.
#[test]
fn a_previous_release_and_version_make_an_update() {
    let params = Params::parse(&with(PREVIOUS)).unwrap();
    assert_eq!(
        params.previous,
        Some(Previous {
            release: PathBuf::from("/var/lib/hostkeeper/packages/tierstore/0.4.0"),
            version: "0.4.0+abcdef012345".to_owned(),
        })
    );
    assert!(!params.first_install());
}

/// Asserts that the previous release and the previous version go together: either one alone is
/// refused. It would catch a rollback that has a release to go back to and no version to look
/// for, or the other way round.
#[test]
fn the_previous_release_and_version_go_together() {
    let release_only = with(PREVIOUS.lines().next().unwrap());
    let version_only = with(PREVIOUS.lines().nth(1).unwrap());
    for text in [release_only, version_only] {
        let why = refused(&text);
        assert!(matches!(why, ParamsError::PreviousPair), "{why}");
        assert!(why.to_string().contains("go together"));
    }
}

/// Asserts that the ceiling is read as a whole number of seconds, and that a negative number, a
/// float and a string are refused with the key named.
#[test]
fn the_promotion_ceiling_is_a_whole_number_of_seconds() {
    let params = Params::parse(&with("\"updater.promotion_ceiling_secs\" = 3600\n")).unwrap();
    assert_eq!(params.promotion_ceiling_secs, Some(3600));
    for bad in ["-5", "1.5", "\"x\""] {
        let why = refused(&with(&format!(
            "\"updater.promotion_ceiling_secs\" = {bad}\n"
        )));
        assert!(
            matches!(why, ParamsError::NotNumber(PROMOTION_CEILING_KEY)),
            "{bad}: {why}"
        );
        assert!(why.to_string().contains("whole number"));
    }
}

/// Asserts the reading of `service_manager`: absent and `"systemd"` are systemd, `"launchd"` is
/// the job whose label is `launchd_label`, and any other value, a string or not, is kept as
/// `Unknown` for every step to refuse. It would catch a typo read as systemd, which would then
/// drive units on a Mac or the other way round.
#[test]
fn the_service_manager_is_read_from_its_key() {
    let manager = |extra: &str| Params::parse(&with(extra)).unwrap().service_manager;
    assert_eq!(manager(""), ServiceManager::Systemd);
    assert_eq!(
        manager("service_manager = \"systemd\"\n"),
        ServiceManager::Systemd
    );
    assert_eq!(
        manager("service_manager = \"launchd\"\nlaunchd_label = \"com.komira.tierstore\"\n"),
        ServiceManager::Launchd {
            label: "com.komira.tierstore".to_owned()
        }
    );
    assert_eq!(
        manager("service_manager = \"foo\"\n"),
        ServiceManager::Unknown("foo".to_owned())
    );
    assert_eq!(
        manager("service_manager = \"Systemd\"\n"),
        ServiceManager::Unknown("Systemd".to_owned())
    );
    assert_eq!(
        manager("service_manager = 5\n"),
        ServiceManager::Unknown("5".to_owned())
    );
    assert_eq!(
        manager("service_manager = true\n"),
        ServiceManager::Unknown("true".to_owned())
    );
}

/// Asserts that a launchd params file needs a label of the plain kind: absent, not a string and
/// every spelling of a label `launchctl` could take for something else are refused. It would
/// catch a label that reaches `launchctl` as an option or a path.
#[test]
fn a_launchd_label_is_required_and_plain() {
    let launchd = |label: &str| format!("service_manager = \"launchd\"\n{label}");
    let why = refused(&with(&launchd("")));
    assert!(
        matches!(why, ParamsError::Missing(LAUNCHD_LABEL_KEY)),
        "{why}"
    );
    assert!(why.to_string().contains("\"launchd_label\" is required"));
    let why = refused(&with(&launchd("launchd_label = 5\n")));
    assert!(
        matches!(why, ParamsError::NotText(LAUNCHD_LABEL_KEY)),
        "{why}"
    );
    for bad in ["", "-x", "a b", "a/b", ".hidden", "sp\\u00e4t"] {
        let why = refused(&with(&launchd(&format!("launchd_label = \"{bad}\"\n"))));
        assert!(
            matches!(why, ParamsError::BadLabel(LAUNCHD_LABEL_KEY)),
            "{bad}: {why}"
        );
        assert!(why.to_string().contains("1 to 255 letters"));
    }
    let long = "a".repeat(MAX_LABEL_LEN + 1);
    let why = refused(&with(&launchd(&format!("launchd_label = \"{long}\"\n"))));
    assert!(matches!(why, ParamsError::BadLabel(_)), "{why}");
    let longest = "a".repeat(MAX_LABEL_LEN);
    assert!(Params::parse(&with(&launchd(&format!("launchd_label = \"{longest}\"\n")))).is_ok());
    // A label is only read for launchd: systemd params may carry any scalar of that name.
    assert!(Params::parse(&with("launchd_label = 5\n")).is_ok());
}

/// Asserts the spelling rule of a label and of a version directly, at their edges.
#[test]
fn labels_and_versions_are_held_to_their_alphabets() {
    assert!(is_label("com.komira.tierstore"));
    assert!(is_label("a_b-c.9"));
    assert!(!is_label(""));
    assert!(!is_label("_a"));
    assert!(!is_label("a:b"));
    assert!(is_version("0.5.0+0123456789ab"));
    assert!(is_version("1.0.0-rc.1~x_y"));
    assert!(!is_version(""));
    assert!(!is_version("-1"));
    assert!(!is_version("a..b"));
    assert!(!is_version("a b"));
    assert!(!is_version("a/b"));
    assert!(is_version(&"1".repeat(MAX_VERSION_LEN)));
    assert!(!is_version(&"1".repeat(MAX_VERSION_LEN + 1)));
}

/// Asserts that `config_file` is read as a plain absolute path and that a relative or dotted one
/// is refused, and that `mode = "restart"` is read as a declared restart while every other
/// value (another word, another case, empty, a number, a boolean) is refused with the key named.
/// It would catch a hook that hands over on a host whose manifest asked for a restart because a
/// typo was read as no preference.
#[test]
fn the_config_file_and_the_mode_are_read() {
    let params = Params::parse(&with("config_file = \"/etc/tierstore/config.toml\"\n")).unwrap();
    assert_eq!(
        params.config_file,
        Some(PathBuf::from("/etc/tierstore/config.toml"))
    );
    for bad in [
        "etc/config.toml",
        "/etc/../config.toml",
        "/etc//config.toml",
        "/etc/",
    ] {
        let why = refused(&with(&format!("config_file = \"{bad}\"\n")));
        assert!(
            matches!(why, ParamsError::NotPlain(CONFIG_FILE_KEY)),
            "{bad}: {why}"
        );
        assert!(
            why.to_string()
                .contains("absolute path of plain components")
        );
    }
    let params = Params::parse(&with("mode = \"restart\"\n")).unwrap();
    assert!(params.declared_restart);
    for bad in [
        "mode = \"handover\"\n",
        "mode = \"Restart\"\n",
        "mode = \"\"\n",
        "mode = 1\n",
        "mode = true\n",
    ] {
        let why = refused(&with(bad));
        assert!(
            matches!(why, ParamsError::BadMode(MODE_KEY)),
            "{bad}: {why}"
        );
        assert!(why.to_string().contains("must be the string \"restart\""));
    }
}

/// Asserts each refusal of the text of a params file and its words: not TOML, a duplicate key, a
/// value that is an array or a table (a dotted key that was not quoted becomes one), a missing
/// required key, a number or a boolean where text is needed, a path that is not plain and a
/// version outside its alphabet.
#[test]
fn a_wrong_params_file_is_refused() {
    let why = refused("updater.staged_release = ");
    assert!(matches!(why, ParamsError::Syntax(_)), "{why}");
    assert!(why.to_string().contains("not a TOML table"));
    assert!(matches!(refused(&with(GOOD)), ParamsError::Syntax(_)));
    let why = refused(&with("list = [1]\n"));
    assert!(
        matches!(&why, ParamsError::NotScalar(key) if key == "list"),
        "{why}"
    );
    assert!(why.to_string().contains("not a scalar"));
    let unquoted = refused("updater.staged_release = \"/a\"\n");
    assert!(matches!(&unquoted, ParamsError::NotScalar(key) if key == "updater"));
    let no_staged = GOOD.lines().nth(1).unwrap().to_owned() + "\n";
    let why = refused(&no_staged);
    assert!(
        matches!(why, ParamsError::Missing(STAGED_RELEASE_KEY)),
        "{why}"
    );
    assert!(
        why.to_string()
            .contains("\"updater.staged_release\" is required")
    );
    let no_version = GOOD.lines().next().unwrap().to_owned() + "\n";
    let why = refused(&no_version);
    assert!(
        matches!(why, ParamsError::Missing(TARGET_VERSION_KEY)),
        "{why}"
    );
    let why = refused(&GOOD.replace("\"0.5.0+0123456789ab\"", "5"));
    assert!(
        matches!(why, ParamsError::NotText(TARGET_VERSION_KEY)),
        "{why}"
    );
    assert!(why.to_string().contains("must be a string"));
    for bad in ["", "-1", "a..b", "a b", "a/b"] {
        let why = refused(&GOOD.replace("0.5.0+0123456789ab", bad));
        assert!(
            matches!(why, ParamsError::BadVersion(TARGET_VERSION_KEY)),
            "{bad}: {why}"
        );
        assert!(why.to_string().contains("not a version"));
    }
    for bad in [
        "releases/abc",
        "/releases/../abc",
        "/releases//abc",
        "/releases/",
        "/releases/./abc",
    ] {
        let why = refused(&GOOD.replace("/var/lib/hostkeeper/packages/tierstore/0.5.0", bad));
        assert!(
            matches!(why, ParamsError::NotPlain(STAGED_RELEASE_KEY)),
            "{bad}: {why}"
        );
    }
    let bad_previous = with(PREVIOUS).replace("0.4.0+abcdef012345", "a b");
    let why = refused(&bad_previous);
    assert!(
        matches!(why, ParamsError::BadVersion(PREVIOUS_VERSION_KEY)),
        "{why}"
    );
    let bad_previous = with(PREVIOUS).replace("/var/lib/hostkeeper/packages/tierstore/0.4.0", "x");
    let why = refused(&bad_previous);
    assert!(
        matches!(why, ParamsError::NotPlain(PREVIOUS_RELEASE_KEY)),
        "{why}"
    );
}

/// Asserts that a file is read from disk and that a missing one is a read error, not an empty
/// table. It would catch a reader that takes an absent params file for no params.
#[test]
fn a_params_file_is_read_from_disk() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-hook-params");
    let path = dir.path().join("params.toml");
    std::fs::write(&path, with(PREVIOUS)).unwrap();
    let params = Params::read(&path).unwrap();
    assert_eq!(params.target_version, "0.5.0+0123456789ab");
    assert!(!params.first_install());
    let missing = Params::read(&dir.path().join("none.toml")).unwrap_err();
    assert!(matches!(missing, ParamsError::Read(_)));
    assert!(missing.to_string().contains("cannot read the params file"));
}
