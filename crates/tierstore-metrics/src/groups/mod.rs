//! The families of section 12, in groups by the component of tierstore that registers them.
//!
//! Each group is declared once, by [`group!`]: a field per family with its type (which fixes its
//! kind and its labels), its name in section 12, its exported name when that differs from
//! `tierstore_` and the design's, its help, and the bucket bounds of a histogram.

mod client;
mod cold;
mod drive;
mod shared;
mod store;

pub use client::{Cache, Fetch, Fuse, Prepare, ReadReply};
pub use cold::{Cold, Movement};
pub use drive::Drives;
pub use shared::{CopyGeneration, Disks, Durability, Process};
pub use store::{Hot, Peer, S3Front};

/// The bucket bounds of a duration, in seconds: 0.5 ms to 5 minutes.
pub(crate) const SECONDS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    60.0, 120.0, 300.0,
];

/// The bucket bounds of a size, in bytes: 4 KiB to 4 GiB by powers of 4.
pub(crate) const BYTES: &[f64] = &[
    4096.0,
    16384.0,
    65536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
    67_108_864.0,
    268_435_456.0,
    1_073_741_824.0,
    4_294_967_296.0,
];

/// The bucket bounds of a count: 1 to 4096 by powers of 2.
pub(crate) const COUNT: &[f64] = &[
    1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0,
];

/// Declares a group: its struct with one public field per family, `new`, and `families`.
macro_rules! group {
    (@name $design:literal) => {
        concat!("tierstore_", $design)
    };
    (@name $design:literal, $exported:literal) => {
        $exported
    };
    (@buckets) => {
        &[]
    };
    (@buckets $buckets:ident) => {
        $crate::groups::$buckets
    };
    (
        $(#[$meta:meta])*
        $name:ident {
            $(
                $field:ident: $ty:ty = $design:literal $(as $exported:literal)?,
                    $help:literal $(, $buckets:ident)?;
            )+
        }
    ) => {
        $(#[$meta])*
        pub struct $name {
            $(
                #[doc = concat!("`", $design, "`: ", $help)]
                pub $field: $ty,
            )+
        }

        impl $name {
            /// The group, with no series yet.
            pub(crate) fn new() -> Self {
                Self {
                    $(
                        $field: <$ty>::new($crate::family::Spec {
                            design: $design,
                            name: $crate::groups::group!(@name $design $(, $exported)?),
                            help: $help,
                            buckets: $crate::groups::group!(@buckets $($buckets)?),
                        }),
                    )+
                }
            }

            /// Makes the seeded series of each family that it has not yet.
            pub(crate) fn seed(&self, seed: &$crate::family::seal::Seed<'_>) {
                $( self.$field.seed(seed); )+
            }

            /// Every family of the group, in the order they render.
            pub(crate) fn families(&self) -> Vec<&dyn $crate::family::Registered> {
                vec![$( &self.$field as &dyn $crate::family::Registered, )+]
            }
        }
    };
}

pub(crate) use group;
