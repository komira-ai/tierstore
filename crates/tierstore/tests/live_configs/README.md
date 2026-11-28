# The host configurations the build farm runs

The files here are tierstore's configuration as the build farm commits it for its hosts, so that a
pull request of this repository that makes the binary refuse, ignore or stop serving something a
farm host's file still uses fails in its own check. The farm keeps its files in a repository of its
own, which this one cannot read; each file here is a copy of one of them, made by the rules below.

| File | The farm's file |
| --- | --- |
| `linux-build-node.toml` | `config.toml` of the Linux build nodes (HostGroup `all-in-one`): S3 on port 9300, the mount provider, and the jobs' trees and put directory as one FUSE mount (the `fs` interface) |
| `mac-build-node.toml` | `tierstore.toml` of the Mac build nodes (HostGroup `mac-build`): S3 on loopback, the mount provider, the trees as one NFS mount on loopback (the `fs` interface), and the put ingest |
| `mac-fs.toml` | `fs.toml` of the Mac build nodes: `[fs] mountpoint` alone, which no process serves from; the config hook reads it at the switch to one process |
| `mac-build-node-before-switch.toml` | `tierstore.toml` of the Mac build nodes before their switch to one process: the mount provider and no `fs` interface, the mount once served by `tierstore fs` |

`sources.sha256` holds the SHA-256 of each farm file a copy was made from, in the form of
`sha256sum`, so that a copy names the version of the farm's file it stands for. A change of a farm
file is copied here by the same rules, with its new line; the file before the switch is kept as it
was. Nothing in this repository can read the farm's files, so nothing here compares the two.

## How a copy is made

From the farm's file, in this order, and nothing else:

1. Every line that is only a comment is removed, and a comment after a value.
2. A run of blank lines is one blank line; none is left at the start or the end.
3. The six addresses of the farm's storage network (the Mac's upstreams) are the documentation
   addresses `192.0.2.1` to `192.0.2.6`, in order.
4. A second cached prefix of the Linux node's `[[policy]]` is named `tools/`.
5. The name of the farm's content-addressed store, the first word of its bucket (`<name>-cas`) and of
   the Linux node's scoped key file (`<name>-cold.key`), is `build`: `build-cas`, `build-cold.key`.

Every key, table, path, account name and number is the farm's.

## What the tests do with them

The tests (`src/server/live_configs.rs` names them) move every path of a copy below a scratch
directory, put the S3 and NFS fronts on ports the system picks, and look the farm's account names up
in a table of the test's own user and group. Each such edit is a literal replacement whose count in
the file is stated, so a copy that gains or loses a path fails the test that reads it. Then:

- `tests/live_configs.rs`: `tierstore --config <copy> --check-config` exits 0 and says nothing, with
  fixture key files; the Mac's file before the switch exits 2 and names its mount provider.
- `src/server/live_configs_tests.rs`: each copy loads with the shape it declares (a build node, its
  mount provider served by its `fs` interface, the put directory or the put ingest).
- `src/server/treefs/live_config_tests.rs` (Linux): the Linux node starts on its copy with a real
  FUSE mount, and a Prepare and Mount on its mount-provider socket answer a directory that lists the
  tree's files, for a tree with an L2 sidecar and one without.
- `src/server/treefs/nfs/live_config_tests.rs`: the Mac node starts on its copy, and a Prepare and
  Mount answer a directory that its NFS front lists with the tree's files.
- `src/server/update_hook/config_hook/steps_tests/live_configs.rs`: the update of a release, and
  the config hook's switch from the layout before, on each copy, over the fake systemd and launchd
  of the hook's tests: each ends with the node serving its mount, or fails and rolls back.
