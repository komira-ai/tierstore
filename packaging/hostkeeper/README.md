# tierstore as a hostkeeper package

hostkeeper installs and updates a service on a host from a package, one archive per version and
platform: `tierstore-<version>-<platform>.tar.zst` for linux-x86_64, linux-arm64 and macos-arm64.
`cargo xtask release hostkeeper-package` builds it from a built binary and the files here
(`docs/ci-and-release.md`, "The hostkeeper package"). The archive's format is hostkeeper's
`docs/package-format.md`; the tests in `crates/xtask/tests/hostkeeper_package` read each package
with hostkeeper's own unpacker, manifest loader and placed-path lint.

| file | in the package | what |
|---|---|---|
| `manifest.toml.in` | `manifest.toml` | the manifest: the version the binary reports, the platform, the oldest hostkeeper, the update hook (`bin/tierstore update-hook`) and its bounds, the units hostkeeper starts, stops and observes, the health URL, and the params the hook reads (`config_file`, `service_manager`, and `launchd_label` on macOS) |
| `units/tierstore@.service` | `units/tierstore@.service` (Linux) | the template of the two instances `tierstore@a.service` and `tierstore@b.service`, which the update hook alternates between with a hand-over of the listening sockets |
| `units/tierstore.plist` | `units/tierstore.plist` (macOS) | the one launchd job, label `tierstore`, which an update restarts |

The binary is `bin/tierstore`, the exact bytes of the build. Every unit runs it as
`/var/lib/hostkeeper/packages/tierstore/current/bin/tierstore`, through the link hostkeeper switches
from one version to the next. The package holds nothing of a node's own: the configuration file
(`/etc/tierstore/config.toml` on Linux, `/usr/local/tierstore/etc/tierstore.toml` on a Mac), the
key files it names and the account (`tierstore`, `_tierstore` on a Mac) are placed on the node
beforehand.

The units are the update hook's (`crates/tierstore/docs/update-hook.md`), which
`crates/tierstore/tests/packaging_hostkeeper.rs` checks against the hook and the binary; the
tests here check the package that carries them.

hostkeeper's manifest loader takes a launchd plist as a unit file and a launchd label as a `run`
entry, and the tests load the macOS manifest and plist with it. The unpacker and loader the tests
and `artifacts.yml` use are those of the commit that merged komira-ai/hostkeeper#23, which know
both the manifest's `platform` key and launchd jobs, so every platform's package is unpacked with
hostkeeper's own code, in the tests and in the build job of its platform.
