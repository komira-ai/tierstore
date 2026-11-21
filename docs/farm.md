# The farm lane: the workspace with Buck2 on the build farm

Cargo is tierstore's build of record. CI's gate (`ci.yml`), the coverage gate and every release
use it, on GitHub-hosted runners, and that does not change. The farm lane builds the same
workspace a second way: with Buck2, through [buildrules](https://github.com/komira-ai/buildrules)'
Rust rules, every action on a remote-execution build farm or taken from its cache. It is
additive and advisory: `.github/workflows/farm.yml` runs it on every push to `main` and every pull
request from a branch of this repository, and no check of it is required.

## What it builds

`buck2 build //:ci`: for every member of the workspace, its `:fmt` (the pinned `rustfmt --check`
over the files `cargo fmt` reads, with `rustfmt.toml`), its `:lint` (clippy over the library, the
binaries and every test target, with `[workspace.lints]` and `clippy.toml`, warnings as errors)
and, for the members not listed below, its `:test`. A test target runs as a build action: its
`.passed` marker exists only if every test of the target passed, and a cached marker means the
tests passed on byte-identical inputs. The summary line must read
`Commands: N (cached: N, remote: N, local: 0)`: no action ran on the runner. `farm.yml` runs the
build as one command, `cargo xtask farm-build` (`crates/xtask/src/farm.rs`): it refuses to start
without the farm's worker property set, fails a build that does not say `local: 0`, and stops
buck2's daemon however the build ended.

The files of the build, all generated or written once by `buildrules`:

| file | what | written by |
|---|---|---|
| `.buckconfig` | the cells, buildrules pinned by commit, the execution platform | `buildrules init`, then by hand (the worker passthrough below) |
| `toolchains/`, `platforms/` | the pinned Rust toolchain and the remote execution platform | `buildrules init` |
| `BUCK`, `crates/*/BUCK` | `cargo_workspace` and one `cargo_package` per member | `buildrules init` and `buildrules cargo adopt`, once; then by hand (`extra_srcs`, below) |
| `buildrules.lock.bzl`, `buildrules.bzl`, `third_party/buildrules/` | Cargo's unit graph of every member, every crates.io crate as a unit pinned by SHA-256 and size, and every git dependency (hostkeeper's crates, the nfsserve fork, the previous tierstore) as GitHub's archive of its pinned commit, by SHA-256 and size (`git_archives.tsv`) | `buildrules sync`; never edited by hand |

`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml` stay the source: `//:check` fails every
target when the generated table no longer matches them, and prints `buildrules sync` as the fix.
After a change to a manifest or the lock, run `buildrules sync` and commit what it wrote.

A file a crate reads beyond its unit's own sources is declared in its BUCK file (`extra_srcs`): a
library sees `src/` and `build.rs`, a test target `tests/`, a build script its whole package. So
a library whose tests include the stored-format fixtures of `tests/`, a test that includes a
benchmark of `examples/`, and a test that reads `docs/`, `packaging/` or another member's fixtures
each name the file or directory. A file read by another member is exported by reference in its
owner's BUCK file (or the root `BUCK`), so it stays at its path.

## What stays in ci.yml, and why

A farm action has its declared inputs and nothing else: no network, no `PATH` but buildrules'
busybox, no git checkout, not root, no `/dev/fuse`, and a `TMPDIR` about 98 bytes deep, too deep
for a Unix socket's path below it (komira-ai/buildrules#170). buildrules' test gate also refuses a
test target that has an `#[ignore]`d test (an ignored test is a test that never runs) or that lists
no test at all. So the tests of nine members run in ci.yml alone. They are format-checked, built
and linted on the farm, and each says so in its `[package.metadata.buildrules.actions]`
(`ci = ["fmt", "lint"]`), with the reason:

| member | why its tests stay hosted |
|---|---|
| `tierstore` | its FUSE tests mount through `/dev/fuse` and `fusermount3`; many tests bind a Unix socket under `TMPDIR` (#170); and some targets hold `#[ignore]`d tests (helper processes, the MinIO tests run by hand) or, on Linux, no test (`tests/fs_macos.rs`) |
| `tsfuse` | its tests mount FUSE file systems through `/dev/fuse` and `fusermount3`, and re-run their own binary with `#[ignore]`d child tests selected |
| `fsbench` | its benchmark tests mount FUSE file systems |
| `tierstore-testkit` | the tests of its socket directories bind a Unix socket under `TMPDIR` (#170) |
| `tierstore-mount-client` | its lifecycle test binds a Unix socket under `TMPDIR` (#170) |
| `tierstore-model` | `tests/matrix.rs` holds two `#[ignore]`d runs of the whole model, run by hand |
| `tierstore-measure` | its one integration test is for systems other than Linux, so on Linux its target lists no test; and its measurements read block devices, `/proc` and `/sys` |
| `tierstore-list` | `tests/golden.rs` holds the `#[ignore]`d `regenerate`, which rewrites the golden fixtures when run by hand |
| `xtask` | its tests read the git checkout (the workflows, `AGENTS.md`, every tracked file) and run git, cargo, sh, bash, jq and tar |

No FUSE test runs on the farm, so none passes there without having mounted anything: a FUSE test
that finds no `/dev/fuse` says so on stderr and ends, which ci.yml's 99% gate would report and the
farm's gate could not.

What else only ci.yml measures, for every member:

- Coverage. The 99% line and branch gate is cargo-llvm-cov's; buildrules has no Rust coverage
  yet.
- Doctests. buildrules does not run them.
- Feature unification across the workspace. ci.yml builds the workspace at once, so a feature one
  member turns on in another is on for all; buildrules builds each member's tests with that
  member's own dependency graph (its dev-dependencies included).

## Running it yourself

You need buck2 (the release in buildrules' `tools/buck2`) and a `.buckconfig.local` (gitignored)
that names the farm: `[buck2_re_client]` with its endpoints and instance, and
`[buildrules_re] linux_x86_64_properties` with its worker property set. This repository holds
neither. Then:

```sh
buck2 build //crates/tierstore-format:test    # one crate's tests
buck2 build //:ci                             # what farm.yml builds
buck2 log what-failed                         # the failing actions and their commands
```

Read the summary line: `local` must be 0.

## How the job reaches the farm

The farm is on a private network. komira's farm-connect action, pinned by commit, joins it with
the job's OIDC token (workload identity federation: no secret is stored), refuses to go on unless
the farm answers (it masks the address in the log), and writes the farm's buckconfig under
`~/.buckconfig.d`, where buck2 reads it. Its inputs are the organisation's variables `TS_CLIENT_ID`
and `TS_AUDIENCE` (the tailnet identity), `FARM_ADDRESS`, `FARM_INSTANCE`,
`FARM_LINUX_X86_64_PROPERTIES` and `FARM_TLS`. farm-connect writes the worker property set under
`[komira_re]`; `.buckconfig` hands it to buildrules (`[buildrules_re] linux_x86_64_properties =
$(config komira_re.linux_x86_64_properties)`) and holds no value of its own.

That join is why the job holds `id-token: write` on a pull request, the one job a pull request
starts that holds a write scope (AGENTS.md, B10 of this repository). The workflow lint
(`crates/xtask/tests/workflows/farm_lane.rs`, and the table of `lint.rs`) holds it to: exactly that
scope and `contents: read`; the exact `if:` that skips a fork's pull request (which gets no OIDC
token) and Dependabot's; farm-connect's inputs exactly the variables above, and farm-connect the
one action pinned by commit with no version comment, in this file alone; a step after
farm-connect's that runs `cargo xtask farm-build` of `//:ci`; and the triggers, one job and one
runner. It does not fix the job's other steps, each of which could ask for the token: a change to
them is reviewed like any change to a workflow.

A pull request's build and a build of `main` share the farm's action cache, as every build of
every repository on the farm does: a result one writes, the other reads. The farm's cache is
keyed by the action's digest, so a result is reused only for byte-identical inputs and command.

## Measured

The first measurements of the lane, of `//:ci` as it is here (buildrules at a693e5c). Wall times
are buck2's or the shell's, on a machine that ran no action itself.

| build | `Commands:` line | wall |
|---|---|---|
| `//:ci`, after earlier runs of the same day had put most units in the farm's cache | `130 (cached: 9, remote: 121, local: 0)`, BUILD SUCCEEDED | 12 min |
| the same, again, daemon kept | no action looked up | 0.04 s |
| the same, again, buck2's daemon restarted (nothing in its memory) | `2001 (cached: 2001, remote: 0, local: 0)` | 1.6 s |

Every git dependency (hostkeeper's crates, the nfsserve fork, the previous tierstore) is built from
GitHub's archive of its pinned commit, and its units are cached like a crates.io crate's.
