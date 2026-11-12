use super::*;
use crate::release::rpm as build;

const FILE: &str = "tierstore-1.2.3-1.x86_64.rpm";

fn bytes(version: &str, arch: &str) -> Vec<u8> {
    build::build(version, arch, &build::files(arch.as_bytes(), b"notice")).expect("build")
}

fn package(version: &str, arch: &str) -> Package {
    let file = format!("tierstore-{version}-1.{arch}.rpm");
    package_of(&file, &bytes(version, arch)).expect("package")
}

/// A package of another product, as `createrepo_c` writes it.
pub(super) fn foreign(name: &str, ver: &str) -> Package {
    let pkgid = format!("{:0>64}", name.len() + ver.len());
    Package {
        name: name.to_owned(),
        arch: "x86_64".to_owned(),
        version: numbers(ver),
        pkgid: pkgid.clone(),
        href: format!("../../pool/{name}/{name}-{ver}-1.x86_64.rpm"),
        primary: format!(
            "<package type=\"rpm\">\n  <name>{name}</name>\n  <arch>x86_64</arch>\n  <version epoch=\"0\" ver=\"{ver}\" rel=\"1\"/>\n  <checksum type=\"sha256\" pkgid=\"YES\">{pkgid}</checksum>\n  <location href=\"../../pool/{name}/{name}-{ver}-1.x86_64.rpm\"/>\n</package>\n"
        ),
        filelists: format!(
            "<package pkgid=\"{pkgid}\" name=\"{name}\" arch=\"x86_64\">\n  <version epoch=\"0\" ver=\"{ver}\" rel=\"1\"/>\n</package>\n"
        ),
        other: format!(
            "<package pkgid=\"{pkgid}\" name=\"{name}\" arch=\"x86_64\">\n  <version epoch=\"0\" ver=\"{ver}\" rel=\"1\"/>\n</package>\n"
        ),
    }
}

#[test]
fn the_chunks_of_a_package_are_what_createrepo_writes() {
    // Catches: a package a dnf client cannot resolve or verify: its name, version and release, the
    // digest of the file (the signed one), the pool location relative to the baseurl, the byte
    // range of the header, the files a dependency may name, and the dependencies once each.
    let file = bytes("1.2.3", "x86_64");
    let p = package_of(FILE, &file).expect("package");
    let pkgid = Sha256::of(&file).to_string();
    let (start, end) = layout::split(&file).expect("parts").header_range();
    let payload = layout::split(&file).expect("parts").payload.len();
    let installed = Rpm::parse(&mut &file[..])
        .expect("rpm")
        .metadata
        .get_installed_size()
        .expect("size");
    let primary = format!(
        "<package type=\"rpm\">
  <name>tierstore</name>
  <arch>x86_64</arch>
  <version epoch=\"0\" ver=\"1.2.3\" rel=\"1\"/>
  <checksum type=\"sha256\" pkgid=\"YES\">{pkgid}</checksum>
  <summary>S3 server over a hot and a cold tier of S3-compatible stores</summary>
  <description>New objects are written to the hot tier, copied to the cold tier in the background,
and freed from the hot tier when it fills.

The package installs the binary, the systemd unit template tierstore@.service and an example
configuration. It starts and enables nothing: write /etc/tierstore/config.toml and its keys.</description>
  <packager>tierstore authors &lt;tierstore@users.noreply.github.com&gt;</packager>
  <url>https://tierstore.dev</url>
  <time file=\"1788220800\" build=\"1788220800\"/>
  <size package=\"{}\" installed=\"{installed}\" archive=\"{payload}\"/>
  <location href=\"../../pool/tierstore/{FILE}\"/>
  <format>
    <rpm:license>Apache-2.0</rpm:license>
    <rpm:vendor></rpm:vendor>
    <rpm:group>Unspecified</rpm:group>
    <rpm:buildhost>localhost</rpm:buildhost>
    <rpm:sourcerpm></rpm:sourcerpm>
    <rpm:header-range start=\"{start}\" end=\"{end}\"/>
    <rpm:provides>
      <rpm:entry name=\"group(tierstore)\"/>
      <rpm:entry name=\"tierstore\" flags=\"EQ\" epoch=\"0\" ver=\"1.2.3\" rel=\"1\"/>
      <rpm:entry name=\"tierstore(x86-64)\" flags=\"EQ\" epoch=\"0\" ver=\"1.2.3\" rel=\"1\"/>
",
        file.len()
    );
    assert!(p.primary.starts_with(&primary), "{}", p.primary);
    assert!(p.primary.ends_with("    <rpm:requires>\n      <rpm:entry name=\"/bin/sh\"/>\n      <rpm:entry name=\"glibc\" flags=\"GE\" epoch=\"0\" ver=\"2.28\"/>\n      <rpm:entry name=\"systemd\"/>\n    </rpm:requires>\n  </format>\n</package>\n"), "{}", p.primary);
    assert!(!p.primary.contains("rpmlib("));
    assert_eq!(
        p.filelists,
        format!(
            "<package pkgid=\"{pkgid}\" name=\"tierstore\" arch=\"x86_64\">
  <version epoch=\"0\" ver=\"1.2.3\" rel=\"1\"/>
  <file>/usr/lib/systemd/system/tierstore@.service</file>
  <file>/usr/lib/sysusers.d/tierstore.conf</file>
  <file type=\"dir\">/usr/libexec/tierstore</file>
  <file>/usr/libexec/tierstore/tierstore</file>
  <file type=\"dir\">/usr/share/doc/tierstore</file>
  <file>/usr/share/doc/tierstore/THIRD-PARTY-LICENSES.txt</file>
  <file type=\"dir\">/usr/share/doc/tierstore/examples</file>
  <file>/usr/share/doc/tierstore/examples/config.toml</file>
  <file type=\"dir\">/usr/share/licenses/tierstore</file>
  <file>/usr/share/licenses/tierstore/LICENSE</file>
</package>
"
        )
    );
    assert_eq!(
        p.other,
        format!(
            "<package pkgid=\"{pkgid}\" name=\"tierstore\" arch=\"x86_64\">\n  <version epoch=\"0\" ver=\"1.2.3\" rel=\"1\"/>\n</package>\n"
        )
    );
    assert_eq!(
        (p.name.as_str(), p.arch.as_str(), p.version),
        ("tierstore", "x86_64", Some((1, 2, 3)))
    );
    assert_eq!(p.href, format!("../../pool/tierstore/{FILE}"));
    assert_eq!(p.pkgid, pkgid);
}

#[test]
fn primary_lists_the_files_a_dependency_may_name_and_no_other() {
    // Catches: a file under /etc or a bin directory left out of primary.xml (dnf would not
    // resolve a dependency on it without the file lists), and another file put in it.
    let mut files = build::files(b"x86_64", b"notice");
    for path in [
        "etc/tierstore/a.toml",
        "usr/bin/a",
        "usr/sbin/b",
        "bin/c",
        "sbin/d",
    ] {
        files.push((path.to_owned(), 0o644, b"x".to_vec()));
    }
    let file = build::build("1.2.3", "x86_64", &files).expect("build");
    let p = package_of(FILE, &file).expect("package");
    assert!(
        p.primary.ends_with(
            "    <file>/bin/c</file>\n    <file>/etc/tierstore/a.toml</file>\n    <file>/sbin/d</file>\n    <file>/usr/bin/a</file>\n    <file>/usr/sbin/b</file>\n  </format>\n</package>\n"
        ),
        "{}",
        p.primary
    );
    assert_eq!(p.primary.matches("<file>").count(), 5, "{}", p.primary);
}

#[test]
fn a_package_is_listed_under_the_name_its_header_says_and_only_if_it_is_one() {
    // Catches: an index that lists a file under a name other than the package's, which dnf
    // would fetch and find to be another.
    let e = package_of("tierstore-9.9.9-1.x86_64.rpm", &bytes("1.2.3", "x86_64"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("is the package tierstore-1.2.3-1.x86_64.rpm"),
        "{e}"
    );
    assert!(package_of(FILE, b"not a package").is_err());
    // A package the layout reads but the rpm crate does not: a lead and two empty headers.
    let mut cut = bytes("1.2.3", "x86_64");
    let (start, _) = layout::split(&cut).expect("parts").header_range();
    cut.truncate(start + 20);
    assert!(package_of(FILE, &cut).is_err());
}

#[test]
fn dependencies_are_written_with_their_comparison_epoch_version_and_release() {
    // Catches: a dependency written without its comparison (satisfied by any version), with the
    // release folded into the version, or an epoch dropped.
    let dep = |flags, version: &str| rpm::Dependency {
        name: "x".to_owned(),
        flags,
        version: version.to_owned(),
    };
    for (flags, op) in [
        (DependencyFlags::LE, "LE"),
        (DependencyFlags::GE, "GE"),
        (DependencyFlags::EQUAL, "EQ"),
        (DependencyFlags::LESS, "LT"),
        (DependencyFlags::GREATER, "GT"),
    ] {
        assert_eq!(
            entry(&dep(flags, "2:1.5-3")),
            format!(
                "      <rpm:entry name=\"x\" flags=\"{op}\" epoch=\"2\" ver=\"1.5\" rel=\"3\"/>\n"
            )
        );
    }
    assert_eq!(
        entry(&dep(DependencyFlags::EQUAL, "1.5")),
        "      <rpm:entry name=\"x\" flags=\"EQ\" epoch=\"0\" ver=\"1.5\"/>\n"
    );
    assert_eq!(
        entry(&dep(DependencyFlags::ANY, "")),
        "      <rpm:entry name=\"x\"/>\n"
    );
    assert_eq!(
        entry(&dep(DependencyFlags::EQUAL, "")),
        "      <rpm:entry name=\"x\"/>\n"
    );
    assert_eq!(
        dependencies("requires", vec![dep(DependencyFlags::ANY, "")])
            .matches("<rpm:entry")
            .count(),
        1
    );
    assert_eq!(dependencies("requires", Vec::new()), "");
    assert_eq!(escape("a&b<c>\"d"), "a&amp;b&lt;c&gt;&quot;d");
}

#[test]
fn the_three_files_are_read_back_into_the_packages_they_were_rendered_from() {
    // Catches: a repository whose packages another product's index run would drop or garble:
    // every chunk survives a render and a parse, the foreign ones byte for byte.
    let all = vec![
        foreign("othertool", "0.9.1"),
        package("1.2.3", "x86_64"),
        foreign("mopm", "2.0.0"),
    ];
    let [primary, filelists, other] = render(&all);
    assert!(primary.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"3\">\n"));
    assert!(primary.ends_with("</package>\n</metadata>\n"));
    assert!(
        filelists.contains(
            "<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\" packages=\"3\">"
        )
    );
    assert!(other.ends_with("</otherdata>\n"));
    let again = parse(&primary, &filelists, &other).expect("parse");
    assert_eq!(again, all);
    let empty = render(&[]);
    assert_eq!(
        parse(&empty[0], &empty[1], &empty[2]).expect("empty"),
        Vec::new()
    );
}

#[test]
fn metadata_that_is_not_in_the_form_createrepo_writes_is_refused_by_what_is_wrong() {
    // Catches: a repository read wrongly and written back with packages missing.
    let all = vec![foreign("othertool", "0.9.1")];
    let [primary, filelists, other] = render(&all);
    let refused = |p: &str, f: &str, o: &str| parse(p, f, o).unwrap_err().to_string();
    assert!(refused("nonsense", &filelists, &other).contains("no root element"));
    assert!(
        refused(
            &primary.replace("</package>\n</metadata>", "</metadata>"),
            &filelists,
            &other
        )
        .contains("without its end")
    );
    assert!(
        refused(
            &primary.replace("<package type", "<other type"),
            &filelists,
            &other
        )
        .contains("something other than a package")
    );
    for (cut, what) in [
        ("<name>othertool</name>", "name"),
        ("<arch>x86_64</arch>", "architecture"),
        ("<version epoch=\"0\" ver=\"0.9.1\" rel=\"1\"/>", "version"),
        ("<checksum type=\"sha256\" pkgid=\"YES\">", "checksum"),
        (
            "<location href=\"../../pool/othertool/othertool-0.9.1-1.x86_64.rpm\"/>",
            "location",
        ),
    ] {
        let e = refused(&primary.replace(cut, ""), &filelists, &other);
        assert!(e.contains(&format!("without its {what}")), "{what}: {e}");
    }
    let empty = render(&[]);
    assert!(refused(&primary, &empty[1], &other).contains("filelists.xml has no package"));
    assert!(refused(&primary, &filelists, &empty[2]).contains("other.xml has no package"));
}

#[test]
fn a_merge_adds_a_new_version_and_changes_nothing_else() {
    // Catches: another product's entry dropped, reordered or re-rendered by a release of this
    // one, and a re-run of the same release listing its package twice.
    let old = package("1.2.3", "x86_64");
    let existing = vec![
        foreign("othertool", "0.9.1"),
        old.clone(),
        foreign("mopm", "2.0.0"),
    ];
    let new = package("1.2.4", "x86_64");
    let merged = merge(existing.clone(), std::slice::from_ref(&new), 25).expect("merge");
    assert_eq!((merged.added, merged.dropped), (1, 0));
    assert_eq!(merged.packages[..3], existing[..]);
    assert_eq!(merged.packages[3], new);
    let again = merge(merged.packages.clone(), std::slice::from_ref(&new), 25).expect("again");
    assert_eq!((again.added, again.dropped), (0, 0));
    assert_eq!(again.packages, merged.packages);
    // Another architecture of the same version is another slot.
    let arm = package("1.2.4", "aarch64");
    assert_eq!(merge(merged.packages, &[arm], 25).expect("arm").added, 1);
}

#[test]
fn a_merge_refuses_to_move_a_channel_down_or_to_list_a_version_with_other_bytes() {
    // Catches: an older release published after a newer one, and a rebuilt package silently
    // replacing the one that was installed and checked.
    let existing = vec![package("1.2.4", "x86_64")];
    let e = merge(existing.clone(), &[package("1.2.3", "x86_64")], 25)
        .unwrap_err()
        .to_string();
    assert!(e.contains("would move the channel down"), "{e}");
    let mut other = package("1.2.4", "x86_64");
    other.pkgid = "f".repeat(64);
    let e = merge(existing, &[other], 25).unwrap_err().to_string();
    assert!(e.contains("listed at this version with other bytes"), "{e}");
    assert!(matches!(
        merge(Vec::new(), &[package("1.2.3", "x86_64")], 0),
        Err(Error::Usage(_))
    ));
    let mut odd = foreign("tierstore", "1.0");
    odd.arch = "x86_64".to_owned();
    let e = merge(vec![odd], &[package("1.2.3", "x86_64")], 25)
        .unwrap_err()
        .to_string();
    assert!(e.contains("other than X.Y.Z"), "{e}");
}

#[test]
fn a_merge_keeps_the_newest_versions_of_the_packages_it_adds_and_leaves_the_rest() {
    // Catches: a history that grows without bound, and a trim that reaches another product's
    // older versions.
    let mut existing = vec![
        foreign("othertool", "0.1.0"),
        foreign("othertool", "0.2.0"),
        foreign("othertool", "0.3.0"),
    ];
    existing.extend(["1.0.0", "1.1.0", "1.2.0"].map(|v| package(v, "x86_64")));
    let merged = merge(existing, &[package("1.3.0", "x86_64")], 2).expect("merge");
    assert_eq!((merged.added, merged.dropped), (1, 2));
    let versions: Vec<_> = merged
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.unwrap_or_default()))
        .collect();
    assert_eq!(
        versions,
        [
            ("othertool", (0, 1, 0)),
            ("othertool", (0, 2, 0)),
            ("othertool", (0, 3, 0)),
            ("tierstore", (1, 2, 0)),
            ("tierstore", (1, 3, 0))
        ]
    );
}

#[test]
fn the_repodata_is_content_named_gzip_and_a_repomd_that_lists_it() {
    // Catches: a repomd whose checksums or sizes are not the files', a name that is not the
    // gzip's digest (the key is created once and never rewritten), and a gzip that differs
    // between runs.
    let packages = vec![foreign("othertool", "0.9.1"), package("1.2.3", "x86_64")];
    let made = files(&packages, 1_790_000_000);
    assert_eq!(made, files(&packages, 1_790_000_000));
    assert_eq!(made.len(), 4);
    assert_eq!(made[3].0, "repomd.xml");
    let repomd = String::from_utf8(made[3].1.clone()).expect("utf8");
    assert!(repomd.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n  <revision>1790000000</revision>\n"));
    let rendered = render(&packages);
    for (i, kind) in ["primary", "filelists", "other"].into_iter().enumerate() {
        let (name, gz) = &made[i];
        let sum = Sha256::of(gz).to_string();
        assert_eq!(name, &format!("{sum}-{kind}.xml.gz"));
        assert_eq!(gunzip(gz).expect("gunzip"), rendered[i].as_bytes());
        assert!(repomd.contains(&format!("  <data type=\"{kind}\">\n    <checksum type=\"sha256\">{sum}</checksum>\n    <open-checksum type=\"sha256\">{}</open-checksum>\n    <location href=\"repodata/{name}\"/>\n    <timestamp>1790000000</timestamp>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n", Sha256::of(rendered[i].as_bytes()), gz.len(), rendered[i].len())));
    }
    let listed = listed(&repomd).expect("listed");
    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed[0],
        (
            "primary".to_owned(),
            made[0].0.clone(),
            Sha256::of(&made[0].1).to_string()
        )
    );
    assert!(gunzip(b"not gzip").is_err());
}

#[test]
fn a_repomd_that_names_a_file_outside_repodata_or_lacks_a_field_is_refused() {
    // Catches: a repomd read from the bucket that makes a reader fetch a key outside the
    // repository's metadata.
    let one = |location: &str| {
        format!(
            "<repomd><data type=\"primary\">\n<checksum type=\"sha256\">ab</checksum>\n<location href=\"{location}\"/>\n</data></repomd>"
        )
    };
    assert_eq!(listed(&one("repodata/x.gz")).expect("listed")[0].1, "x.gz");
    for bad in [
        "../x.gz",
        "repodata/",
        "repodata/a/b.gz",
        "repodata/.hidden",
        "other/x.gz",
    ] {
        let e = listed(&one(bad)).unwrap_err().to_string();
        assert!(e.contains("is not a file of repodata/"), "{bad}: {e}");
    }
    let e = listed(
        "<repomd><data type=\"primary\">\n<checksum type=\"sha256\">ab</checksum></data></repomd>",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("without its location"), "{e}");
    let e =
        listed("<repomd><data type=\"primary\">\n<location href=\"repodata/x\"/></data></repomd>")
            .unwrap_err()
            .to_string();
    assert!(e.contains("without its checksum"), "{e}");
    assert_eq!(listed("<repomd></repomd>").expect("none"), Vec::new());
}

#[test]
fn a_field_the_package_lacks_is_named_in_the_failure() {
    // Catches: a package listed with a field missing, or an error that does not say which.
    let e = field("build time", Err::<u64, _>(rpm::Error::TagNotFound(1))).unwrap_err();
    assert!(e.to_string().starts_with("read the build time: "), "{e}");
    assert_eq!(field("name", Ok::<_, rpm::Error>(7)).expect("a field"), 7);
}
