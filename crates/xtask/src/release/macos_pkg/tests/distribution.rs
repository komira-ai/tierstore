//! The `Distribution` layer of the product: its component and its elements.

use super::*;

/// The problems of the `Distribution` `text` of the pkg of 1.2.3.
fn problems_of(text: &str) -> Vec<String> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let expanded = Expanded::good(&format!("macos-pkg-distribution-{n}"));
    std::fs::write(expanded.product().join(DISTRIBUTION), text).expect("the distribution");
    match expanded.check() {
        Ok(_) => Vec::new(),
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a pass or a refusal, got {other:?}"),
    }
}

#[test]
fn the_distribution_productbuild_writes_for_the_component_passes() {
    // Catches: a Distribution check that refuses the file productbuild synthesizes, or one that
    // needs more of it than the identifier, the version and the component's reference.
    assert_eq!(problems_of(&distribution_text()), Vec::<String>::new());
    // The reference may be written with its text on its own lines.
    let spaced = distribution_with(
        &COMPONENT_REF
            .replace(">#", ">\n  #")
            .replace("</pkg", "\n</pkg"),
        "",
    );
    assert_eq!(problems_of(&spaced), Vec::<String>::new());
}

#[test]
fn a_distribution_that_names_another_component_id_is_refused() {
    // Catches: a product that installs a second component, or one under another identifier.
    let id = "com.komira.tierstore";
    let second = format!("<pkg-ref id=\"com.komira.other\"/>\n    {COMPONENT_REF}");
    assert_eq!(
        problems_of(&distribution_with(&second, "")),
        [format!(
            "Distribution: a <pkg-ref> has the id Some(\"com.komira.other\"), not {id:?}: the \
             product holds only the one component"
        )]
    );
    let none = distribution_with("<pkg-ref/>\n    ", "").replace(COMPONENT_REF, "");
    assert_eq!(
        problems_of(&none),
        [
            format!(
                "Distribution: a <pkg-ref> has the id None, not {id:?}: the product holds only \
                 the one component"
            ),
            "Distribution has 0 <pkg-ref> elements that refer to #tierstore.pkg, not one"
                .to_owned(),
        ]
    );
}

#[test]
fn a_distribution_with_another_component_file_or_a_second_reference_is_refused() {
    // Catches: a Distribution that points at a package the product does not hold, or at the
    // component twice.
    let other = COMPONENT_REF.replace("#tierstore.pkg", "#other.pkg");
    assert_eq!(
        problems_of(&distribution_with(&other, "")),
        [
            "Distribution: a <pkg-ref> refers to \"#other.pkg\", not \"#tierstore.pkg\", the \
             component of the product archive"
                .to_owned(),
            "Distribution has 0 <pkg-ref> elements that refer to #tierstore.pkg, not one"
                .to_owned(),
        ]
    );
    let twice = format!("{COMPONENT_REF}\n    {COMPONENT_REF}");
    assert_eq!(
        problems_of(&distribution_with(&twice, "")),
        ["Distribution has 2 <pkg-ref> elements that refer to #tierstore.pkg, not one"]
    );
}

#[test]
fn a_distribution_of_another_version_or_conclusion_is_refused() {
    // Catches: a product of another release, and one that asks the installer to restart, log out
    // or shut down after the install.
    let old = COMPONENT_REF.replace("version=\"1.2.3\"", "version=\"1.2.4\"");
    assert_eq!(
        problems_of(&distribution_with(&old, "")),
        [
            "Distribution: the <pkg-ref> of #tierstore.pkg has the version Some(\"1.2.4\"), not \
             \"1.2.3\""
        ]
    );
    let bare = COMPONENT_REF.replace(" version=\"1.2.3\"", "");
    assert_eq!(
        problems_of(&distribution_with(&bare, "")),
        ["Distribution: the <pkg-ref> of #tierstore.pkg has the version None, not \"1.2.3\""]
    );
    let restart = COMPONENT_REF.replace("onConclusion=\"none\"", "onConclusion=\"RequireRestart\"");
    assert_eq!(
        problems_of(&distribution_with(&restart, "")),
        ["Distribution: a <pkg-ref> has onConclusion \"RequireRestart\", not \"none\""]
    );
    // A reference without onConclusion is the installer's default, none.
    let default = COMPONENT_REF.replace(" onConclusion=\"none\"", "");
    assert_eq!(
        problems_of(&distribution_with(&default, "")),
        Vec::<String>::new()
    );
}

#[test]
fn a_distribution_without_its_product_or_with_another_product_is_refused() {
    // Catches: a Distribution with no product element, or one of another identifier or version.
    let good = distribution_text();
    let product = "<product id=\"com.komira.tierstore\" version=\"1.2.3\"/>";
    assert!(good.contains(product));
    assert_eq!(
        problems_of(&good.replace(product, "")),
        ["Distribution has no <product> element"]
    );
    assert_eq!(
        problems_of(&good.replace(
            product,
            "<product id=\"com.komira.other\" version=\"1.2.4\"/>"
        )),
        [
            "Distribution: the <product> id is Some(\"com.komira.other\"), not \
             \"com.komira.tierstore\"",
            "Distribution: the <product> version is Some(\"1.2.4\"), not \"1.2.3\"",
        ]
    );
    assert_eq!(
        problems_of(&good.replace(product, "<product/>")),
        [
            "Distribution: the <product> id is None, not \"com.komira.tierstore\"",
            "Distribution: the <product> version is None, not \"1.2.3\"",
        ]
    );
    // An element that only starts with the name is another element.
    assert_eq!(
        problems_of(&good.replace(product, "<products/>")),
        ["Distribution has no <product> element"]
    );
}

#[test]
fn a_distribution_with_installer_code_or_a_check_or_no_root_is_refused() {
    // Catches: installer JavaScript (a script element, an installation or volume check) that
    // would run on the Mac at install, and a file that is not a Distribution.
    for forbidden in ["script", "installation-check", "volume-check"] {
        let with = distribution_with(COMPONENT_REF, &format!("<{forbidden}>x</{forbidden}>"));
        assert_eq!(
            problems_of(&with),
            [format!(
                "Distribution has a <{forbidden}> element: no installer JavaScript or install \
                 check runs"
            )]
        );
    }
    assert_eq!(
        problems_of(&distribution_text().replace("installer-gui-script", "installer")),
        ["Distribution has no <installer-gui-script> element"]
    );
}

#[test]
fn a_distribution_with_an_unreadable_start_tag_is_refused() {
    // Catches: a start tag read loosely, so that a malformed pkg-ref or product passes.
    let broken = distribution_text().replace(COMPONENT_REF, "<pkg-ref id=dev>#tierstore.pkg");
    assert_eq!(
        problems_of(&broken),
        ["Distribution has an unreadable <pkg-ref> element"]
    );
    let broken = distribution_text().replace("<product id=", "<product id=x ");
    assert_eq!(
        problems_of(&broken),
        ["Distribution has an unreadable <product> element"]
    );
    let nameless = distribution_text().replace("<product id=", "<product =\"x\" id=");
    assert_eq!(
        problems_of(&nameless),
        ["Distribution has an unreadable <product> element"]
    );
}

#[test]
fn a_reference_text_that_never_ends_is_read_to_the_end_of_the_file() {
    // Catches: a scan that panics or drops a reference whose text has no tag after it.
    let ends = "<installer-gui-script><product id=\"com.komira.tierstore\" \
                version=\"1.2.3\"/><pkg-ref id=\"com.komira.tierstore\" \
                version=\"1.2.3\">#tierstore.pkg";
    assert_eq!(problems_of(ends), Vec::<String>::new());
}
