use super::super::{check, code, parse, read_docs, said, with_appended};
use super::{LINUX_ONLY, OFF_LINUX_REFUSAL, judged};
use crate::server::config_testing::{DEFAULTS, loads};

/// A whole file that turns the put directory on, as the page's example does.
const PUT_FILE: &str = "[fs]\nclient_group = \"builders\"\nowner = \"1000:1000\"\n\
                        fill_socket = \"/var/lib/tierstore/fill.sock\"\n\
                        cache_path = \"/var/cache/tierstore\"\n\n[fs.put]\nenabled = true\n\
                        prefixes = [\"cas/b/\"]\nstage_dir = \"/var/cache/tierstore/put-stage\"\n";

fn refused(why: &str) -> Result<(), String> {
    Err(why.to_owned())
}

/// Asserts that an unmarked ```` ```toml ```` file must load on every system: taken it is no
/// problem, and refused it is, with the loader's words, the put refusal off Linux included (the
/// fault of the put example before it was marked).
#[test]
fn an_unmarked_file_must_load_everywhere() {
    for on_linux in [true, false] {
        assert_eq!(judged(Ok(()), false, on_linux), None);
        assert_eq!(
            judged(refused("server.x: wrong"), false, on_linux).as_deref(),
            Some("the loader refuses the file: server.x: wrong")
        );
    }
    assert_eq!(
        judged(refused(OFF_LINUX_REFUSAL), false, false),
        Some(format!("the loader refuses the file: {OFF_LINUX_REFUSAL}"))
    );
}

/// Asserts that on Linux a file marked `toml linux` must load: any refusal is a problem, the put
/// refusal too (the loader never gives it on Linux).
#[test]
fn a_linux_file_must_load_on_linux() {
    assert_eq!(judged(Ok(()), true, true), None);
    assert_eq!(
        judged(refused(OFF_LINUX_REFUSAL), true, true),
        Some(format!("the loader refuses the file: {OFF_LINUX_REFUSAL}"))
    );
}

/// Asserts that off Linux a file marked `toml linux` must be refused with exactly the put
/// refusal: the message with a word more before or after it, or another message, is a problem, and
/// a file the loader takes is a problem (the mark is stale).
#[test]
fn a_linux_file_must_be_refused_off_linux_for_the_put_directory_alone() {
    assert_eq!(judged(refused(OFF_LINUX_REFUSAL), true, false), None);
    let other = "the loader refuses the file marked `toml linux` off Linux, and not for the put \
                 directory: ";
    for why in [
        "fs.cache_path: must be an absolute path".to_owned(),
        format!("{OFF_LINUX_REFUSAL} "),
        format!("x{OFF_LINUX_REFUSAL}"),
        String::new(),
    ] {
        assert_eq!(
            judged(Err(why.clone()), true, false),
            Some(format!("{other}{why}"))
        );
    }
    assert_eq!(
        judged(Ok(()), true, false).as_deref(),
        Some(
            "the loader takes the file marked `toml linux` off Linux, where it must refuse it \
             with: fs.put.enabled: the put directory is served by the FUSE frontend (Linux) \
             alone in this release"
        )
    );
}

/// Asserts that the refusal the check expects is the one the loader holds: the key it names and
/// the message are in the source (on Linux too, where the loader never says it), and a file that
/// turns the put directory on gets what this system must give it from the real loader.
#[test]
fn the_refusal_is_the_loaders_and_the_put_file_is_judged_by_it() {
    let (key, message) = OFF_LINUX_REFUSAL
        .split_once(": ")
        .expect("the refusal names its key");
    let source = code().source;
    assert!(source.contains(&format!("\"{key}\"")), "{key}");
    assert!(source.contains(&format!("\"{message}\"")), "{message}");
    let on_linux = cfg!(target_os = "linux");
    assert_eq!(judged(loads(PUT_FILE, DEFAULTS), true, on_linux), None);
}

/// Asserts that the put example of the configuration page is marked `toml linux`, so it is the
/// block the check judges by this system.
#[test]
fn the_put_example_of_the_page_is_marked_linux() {
    let docs = read_docs();
    let page = docs
        .iter()
        .find(|d| d.name == "configuration.md")
        .expect("the page exists");
    let marked: Vec<_> = parse(&page.text)
        .blocks
        .into_iter()
        .filter(|b| b.info == LINUX_ONLY)
        .collect();
    assert_eq!(marked.len(), 1, "the blocks marked {LINUX_ONLY}");
    assert!(marked[0].body.contains("[fs.put]\nenabled = true\n"));
}

/// Asserts that the checker of the pages judges a block marked `toml linux`: the put file is no
/// problem on this system, a file the loader refuses for a fault of its `[fs.put]` (a relative
/// `stage_dir`, refused before the system is looked at) is found on every system, and the keys of
/// a marked block are checked as those of any TOML block are.
#[test]
fn the_checker_judges_a_block_marked_linux() {
    let fenced = |body: &str| format!("\n```{LINUX_ONLY}\n{body}```\n");
    let problems = check(
        &with_appended("configuration.md", &fenced(PUT_FILE)),
        &code(),
    );
    assert!(!said(&problems, "the loader"), "{problems:?}");
    let broken = PUT_FILE.replace("\"/var/cache/tierstore/put-stage\"", "\"put-stage\"");
    let problems = check(
        &with_appended("configuration.md", &fenced(&broken)),
        &code(),
    );
    assert!(
        said(&problems, "the loader refuses the file"),
        "{problems:?}"
    );
    let misspelt = PUT_FILE.replace("owner =", "ownerr =");
    let problems = check(
        &with_appended("configuration.md", &fenced(&misspelt)),
        &code(),
    );
    assert!(said(&problems, "fs.ownerr is not a key"), "{problems:?}");
}
