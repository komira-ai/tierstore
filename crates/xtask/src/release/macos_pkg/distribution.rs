//! The distribution layer of the product archive: the `Distribution` file `productbuild`
//! synthesizes around the one component pkg (`--package`), read back by hand-scanning its XML as
//! `PackageInfo` is, with no XML dependency.
//!
//! The product archive `pkgutil --expand` expands holds the file [`DISTRIBUTION`] and the
//! directory [`COMPONENT`], the component pkg expanded. What `Distribution` must say is fixed by
//! [`distribution_problems`]: the root element, one component named by [`COMPONENT`] under the
//! pkg's identifier and version, the product, and no installer JavaScript or install-time check.
//! The names and the forbidden elements below are the layout `productbuild` writes; a difference
//! on a runner is a change to these constants alone.

use std::collections::BTreeMap;

/// The name of the component pkg: the file `pkgbuild` writes into the stage, the directory of the
/// expanded product archive that holds it, and the target of the `Distribution`'s one reference
/// (`#tierstore.pkg`).
pub(super) const COMPONENT: &str = "tierstore.pkg";

/// The name of the file `productbuild` writes beside the component in the product archive.
pub(super) const DISTRIBUTION: &str = "Distribution";

/// The root element of a `Distribution`.
const ROOT_ELEMENT: &str = "installer-gui-script";

/// The elements that make the installer run code or a check of its own: none may appear, so no
/// installer JavaScript runs.
const FORBIDDEN_ELEMENTS: [&str; 3] = ["script", "installation-check", "volume-check"];

/// The one value of `onConclusion` a reference may carry: the installer does nothing after the
/// install (no restart, logout or shutdown).
const CONCLUSION_NONE: &str = "none";

/// One element of the `Distribution`: its attributes and the text that follows its start tag up
/// to the next tag (empty for a self-closing element).
#[derive(Debug, PartialEq, Eq)]
struct Element {
    attributes: BTreeMap<String, String>,
    text: String,
}

impl Element {
    /// The value of the attribute `name`, if it is there.
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }
}

/// The attributes of the start tag whose name ends just before `rest`, the text after the tag,
/// and whether the tag closes itself (`/>`); none when the attributes are not `name="value"`
/// pairs or the tag does not end.
pub(super) fn open_tag(mut rest: &str) -> Option<(BTreeMap<String, String>, &str, bool)> {
    let mut found = BTreeMap::new();
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("/>") {
            return Some((found, after, true));
        }
        if let Some(after) = rest.strip_prefix('>') {
            return Some((found, after, false));
        }
        let (name, after) = rest.split_once("=\"")?;
        let (value, after) = after.split_once('"')?;
        let name = name.trim();
        let well_formed = !name.is_empty()
            & name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() | matches!(b, b'-' | b'_' | b':'));
        if !well_formed {
            return None;
        }
        found.insert(name.to_owned(), value.to_owned());
        rest = after;
    }
}

/// Every element named `name` in `text`, in order (`<name` followed by a space, `>` or `/`, so
/// `<name-x` is another element); none when one of their start tags cannot be read.
fn elements(text: &str, name: &str) -> Option<Vec<Element>> {
    let open = format!("<{name}");
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        rest = &rest[at + open.len()..];
        if !rest.starts_with(|c: char| c.is_whitespace() | matches!(c, '>' | '/')) {
            continue;
        }
        let (attributes, after, closed) = open_tag(rest)?;
        let body = after.find('<').map_or(after, |end| &after[..end]);
        found.push(Element {
            attributes,
            text: if closed {
                String::new()
            } else {
                body.trim().to_owned()
            },
        });
        rest = after;
    }
    Some(found)
}

/// What is wrong with the `Distribution` text `text` of a product of the one component file
/// `component`, whose identifier is `who` and version `version`, one line each.
pub(crate) fn product_problems(
    text: &str,
    who: &str,
    component: &str,
    version: &str,
    problems: &mut Vec<String>,
) {
    let (who, version) = (who.to_owned(), version.to_owned());
    if !text.contains(&format!("<{ROOT_ELEMENT}")) {
        problems.push(format!("Distribution has no <{ROOT_ELEMENT}> element"));
    }
    for forbidden in FORBIDDEN_ELEMENTS {
        if text.contains(&format!("<{forbidden}")) {
            problems.push(format!(
                "Distribution has a <{forbidden}> element: no installer JavaScript or install \
                 check runs"
            ));
        }
    }
    match elements(text, "pkg-ref") {
        None => problems.push("Distribution has an unreadable <pkg-ref> element".to_owned()),
        Some(refs) => pkg_ref_problems(&refs, &who, component, &version, problems),
    }
    match elements(text, "product") {
        None => problems.push("Distribution has an unreadable <product> element".to_owned()),
        Some(products) if products.is_empty() => {
            problems.push("Distribution has no <product> element".to_owned());
        }
        Some(products) => {
            for product in &products {
                for (name, want) in [("id", &who), ("version", &version)] {
                    let got = product.attribute(name);
                    if got != Some(want.as_str()) {
                        problems.push(format!(
                            "Distribution: the <product> {name} is {got:?}, not {want:?}"
                        ));
                    }
                }
            }
        }
    }
}

/// What is wrong with the `<pkg-ref>` elements `refs`: each names the identifier and no other
/// component and concludes with nothing; exactly one refers to the component file, at the
/// version.
fn pkg_ref_problems(
    refs: &[Element],
    who: &str,
    component: &str,
    version: &str,
    problems: &mut Vec<String>,
) {
    let target = format!("#{component}");
    for pkg_ref in refs {
        let id = pkg_ref.attribute("id");
        if id != Some(who) {
            problems.push(format!(
                "Distribution: a <pkg-ref> has the id {id:?}, not {who:?}: the product holds \
                 only the one component"
            ));
        }
        if let Some(conclusion) = pkg_ref.attribute("onConclusion")
            && conclusion != CONCLUSION_NONE
        {
            problems.push(format!(
                "Distribution: a <pkg-ref> has onConclusion {conclusion:?}, not \
                 {CONCLUSION_NONE:?}"
            ));
        }
        if !pkg_ref.text.is_empty() && pkg_ref.text != target {
            problems.push(format!(
                "Distribution: a <pkg-ref> refers to {:?}, not {target:?}, the component of the \
                 product archive",
                pkg_ref.text
            ));
        }
    }
    let components: Vec<&Element> = refs.iter().filter(|r| r.text == target).collect();
    match components.as_slice() {
        [one] => {
            let got = one.attribute("version");
            if got != Some(version) {
                problems.push(format!(
                    "Distribution: the <pkg-ref> of {target} has the version {got:?}, not \
                     {version:?}"
                ));
            }
        }
        other => problems.push(format!(
            "Distribution has {} <pkg-ref> elements that refer to {target}, not one",
            other.len()
        )),
    }
}
