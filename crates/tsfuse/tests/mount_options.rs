//! The rendering of mount options (5.2, item 6), ported from the fork's
//! `src/mnt/mount_options.rs` (`option_checking`, `option_round_trip`) to the smaller option set:
//! each option is written in the `mount(2)` data string or the `fusermount3 -o` string, and a
//! value that would break the string is refused.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

use tsfuse::mount::{MountError, MountOptions};

#[test]
fn a_tree_mount_renders_every_option_of_5_2() {
    let tree = MountOptions::tree("tierstore");
    assert_eq!(
        tree.kernel_data(7, 1000, 1001),
        "fd=7,rootmode=40000,user_id=1000,group_id=1001,default_permissions,allow_other"
    );
    assert_eq!(
        tree.fusermount_options(),
        "ro,nosuid,nodev,default_permissions,allow_other,fsname=tierstore,subtype=tsfuse"
    );
    assert_eq!(tree.fstype(), "fuse.tsfuse");
    tree.check().unwrap();
}

#[test]
fn the_put_mount_is_the_tree_mount_writable() {
    let put = MountOptions::put("tierstore-put");
    assert_eq!(
        put.fusermount_options(),
        "nosuid,nodev,default_permissions,allow_other,fsname=tierstore-put,subtype=tsfuse"
    );
    assert_eq!(
        MountOptions {
            read_only: true,
            ..put
        },
        MountOptions::tree("tierstore-put")
    );
}

#[test]
fn each_option_is_written_only_when_set() {
    let none = MountOptions {
        read_only: false,
        nosuid: false,
        nodev: false,
        default_permissions: false,
        allow_other: false,
        fsname: "f".to_owned(),
        subtype: None,
    };
    assert_eq!(
        none.kernel_data(3, 0, 0),
        "fd=3,rootmode=40000,user_id=0,group_id=0"
    );
    assert_eq!(none.fusermount_options(), "fsname=f");
    assert_eq!(none.fstype(), "fuse");
    none.check().unwrap();
    // Each flag alone, in both strings.
    type Set = fn(&mut MountOptions);
    let cases: [(Set, &str, &str); 5] = [
        (|o| o.read_only = true, "", "ro,"),
        (|o| o.nosuid = true, "", "nosuid,"),
        (|o| o.nodev = true, "", "nodev,"),
        (
            |o| o.default_permissions = true,
            ",default_permissions",
            "default_permissions,",
        ),
        (|o| o.allow_other = true, ",allow_other", "allow_other,"),
    ];
    for (set, data, helper) in cases {
        let mut one = none.clone();
        set(&mut one);
        assert_eq!(
            one.kernel_data(3, 0, 0),
            format!("fd=3,rootmode=40000,user_id=0,group_id=0{data}")
        );
        assert_eq!(one.fusermount_options(), format!("{helper}fsname=f"));
    }
}

#[test]
fn a_value_that_would_break_the_option_string_is_refused() {
    for bad in ["", "a,b", "a=b", "a\0b", "a b", "a\tb"] {
        for subtype in [false, true] {
            let mut options = MountOptions::tree("ok");
            match subtype {
                true => options.subtype = Some(bad.to_owned()),
                false => options.fsname = bad.to_owned(),
            }
            let err = options.check().unwrap_err();
            let MountError::InvalidOption { option, value } = &err else {
                panic!("{err:?}");
            };
            assert_eq!(*option, ["fsname", "subtype"][usize::from(subtype)]);
            assert_eq!(value, bad);
            assert_eq!(
                err.to_string(),
                format!(
                    "mount option {option}={bad:?} is empty or holds a comma, '=', NUL or whitespace"
                )
            );
        }
    }
}
