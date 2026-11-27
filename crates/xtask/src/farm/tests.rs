use super::*;

#[test]
fn the_properties_are_set_only_by_a_value_that_is_not_blank() {
    // Catches: the key's absence (`{}`, what buck2 prints for an unset key), an empty or blank
    // value, another key, a value that is not text, or output that is not JSON read as set.
    assert!(has_value(
        r#"{"buildrules_re.linux_x86_64_properties":"OSFamily=Linux,ISA=x86-64"}"#
    ));
    for json in [
        "{}",
        r#"{"buildrules_re.linux_x86_64_properties":""}"#,
        r#"{"buildrules_re.linux_x86_64_properties":"  "}"#,
        r#"{"buildrules_re.linux_aarch64_properties":"OSFamily=Linux"}"#,
        r#"{"buildrules_re.linux_x86_64_properties":["OSFamily=Linux"]}"#,
        "not json",
        "",
    ] {
        assert!(!has_value(json), "{json}");
    }
}

#[test]
fn every_error_says_what_is_wrong() {
    // Catches: an error whose words do not name the program, the key or the status.
    let cases = [
        (
            Error::Start {
                program: PathBuf::from("/x/buck2"),
                why: "No such file or directory (os error 2)".to_owned(),
            },
            "cannot run \"/x/buck2\": No such file or directory (os error 2)",
        ),
        (
            Error::Audit {
                status: "exit status: 3".to_owned(),
                stderr: "no .buckconfig".to_owned(),
            },
            "`buck2 audit config` failed (exit status: 3): no .buckconfig",
        ),
        (
            Error::NoWorkers,
            "buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no [komira_re] \
             linux_x86_64_properties, so buck2 has no farm to build on",
        ),
    ];
    for (error, words) in cases {
        assert_eq!(error.to_string(), words);
    }
}
