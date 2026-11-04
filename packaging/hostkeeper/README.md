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

The update hook of this tree still drives the slot units of `packaging/tierstore@.service`; the
a/b instances, the hand-over through `server.handoff_socket` and the `[params]` the manifest names
are its rewrite for hostkeeper, which has not landed. Until it has, no host runs this package.

hostkeeper's manifest loader does not yet take a launchd plist as a unit file or a launchd label as
a `run` entry, so it refuses the macOS package today; the test that loads it waits, ignored, until
it does.
