# The farm lane: the workspace with Buck2 on the build farm

Cargo is tierstore's build of record. CI's gate (`ci.yml`), the coverage gate and every release
use it, on GitHub-hosted runners, and that does not change. The farm lane builds the same
workspace a second way: with Buck2, through [buildrules](https://github.com/komira-ai/buildrules)'
Rust rules, every action on a remote-execution build farm or taken from its cache. It is
additive and advisory: `.github/workflows/farm.yml` runs it on every push to `main` and every pull
request from a branch of this repository (not a fork's, not Dependabot's), and no check of it is
required.

## What it builds

`buck2 build //:ci`: for every member of the workspace, its `:fmt` (the pinned `rustfmt --check`
over the files `cargo fmt` reads, with `rustfmt.toml`), its `:lint` (clippy over the library, the
binaries and every test target, with `[workspace.lints]` and `clippy.toml`, warnings as errors)
and, for the members not listed below, its `:test`. A test target runs as a build action: its
`.passed` marker exists only if every test of the target passed, and a cached marker means the
tests passed on byte-identical inputs. The summary line must read
`Commands: N (cached: N, remote: N, local: 0)`: no action ran on the runner. `farm.yml` first runs
`cargo xtask farm-check` (`crates/xtask/src/farm.rs`), which refuses to go on without the farm's
worker property set, then the build through komira's buck2-farm action (below), which fails a
build that does not say `local: 0`, and last stops buck2's daemon however the build ended.

The files of the build, all generated or written once by `buildrules`:

| file | what | written by |
|---|---|---|
| `.buckconfig` | the cells, buildrules pinned by commit, the execution platform | `buildrules init`, then by hand (the worker passthrough below) |
| `toolchains/`, `platforms/` | the pinned Rust toolchain and the remote execution platform | `buildrules init` |
| `BUCK`, `crates/*/BUCK` | `cargo_workspace` and one `cargo_package` per member | `buildrules init` and `buildrules cargo adopt`, once; then by hand (`extra_srcs`, below) |
| `buildrules.lock.bzl`, `buildrules.bzl`, `third_party/buildrules/` | Cargo's unit graph of every member, every crates.io crate as a unit pinned by SHA-256 and size, and every git dependency (hostkeeper's crates, the nfsserve fork, the two pinned earlier releases of tierstore) as GitHub's archive of its pinned commit, by SHA-256 and size (`git_archives.tsv`) | `buildrules sync`; never edited by hand |

`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml` stay the source: `//:check` fails every
target when the generated table no longer matches them, and prints `buildrules sync` as the fix
(with `STALE TABLE ONLY: ...` when the table alone is stale). After a change to a manifest or the
lock, either run `buildrules sync` and commit what it wrote, or take the patch the farm lane
offers (below).

### A stale table in a pull request

When the build of `farm.yml` fails, four more steps of the same job, as buildrules' docs/how.md
gives them, build buildrules' `//:sync_patch` through buck2-farm: `buildrules sync` as a farm
action, with no network (the sources `Cargo.lock` pins, fetched by hash), whose output is what the
sync would change rather than the change. They upload `offer/` as the run's artifact `sync-patch`
(`sync.patch`, one patch `git apply` takes, and `files/`, the regenerated files themselves), build
buildrules' summary tool on the farm (the doc's `buck2 run` of it, through buck2-farm instead) and
run it on the runner, which writes the verdict to the job's summary: `none`
(the generated files are current, and the failure is elsewhere), `tables` (only the generated
files are stale) or `lock` (cargo changed `Cargo.lock` too, and the patch holds that
`Cargo.lock`: read that part before you commit it). Nothing is pushed, and the job stays red.

For the verdicts `tables` and `lock`, the summary gives two commands. Run them at the root of a
checkout of the pull request's branch, then commit what changed and push:

```sh
gh run download <run> -R komira-ai/tierstore -n sync-patch -D ../sync-patch-<run>
git apply ../sync-patch-<run>/sync.patch
```

So a pull request that adds or bumps a dependency needs no `buildrules sync` on a workstation: push
the `Cargo.toml` change with the `Cargo.lock` that cargo writes for it (any `cargo` command of the
workspace does), wait for the farm lane, and take its patch. A pull request's run builds the
branch merged into `main` (the `pull_request` checkout), so the patch is made for that merge: when
the branch is behind `main`, merge `main` into it and push first, and take the patch of that run.
After any other push that touches a manifest, take the patch of the newer run.

What the farm sync cannot do, the first step says in its log, and the summary says there is no
patch; these need `buildrules sync` where there is a network:

- a crate or a git commit that `Cargo.lock` does not name yet (a `Cargo.toml` changed without the
  `Cargo.lock` cargo writes for it);
- a member the table does not list yet: its package cannot load until the table lists it;
- a change of `rust-toolchain.toml`: the sync runs the Rust of the table.

A fork's pull request and Dependabot's get no farm-lane run (below), so no patch either.

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
no test at all. So the tests of ten members run in ci.yml alone. They are format-checked, built
and linted on the farm, and each says so in its `[package.metadata.buildrules.actions]`
(`ci = ["fmt", "lint"]`), with the reason:

| member | why its tests stay hosted |
|---|---|
| `tierstore` | its FUSE tests mount through `/dev/fuse` and `fusermount3`; many tests bind a Unix socket under `TMPDIR` (#170); and some targets hold `#[ignore]`d tests (helper processes, the MinIO tests run by hand) or, on Linux, no test (`tests/fs_macos.rs`) |
| `tsfuse` | its tests mount FUSE file systems through `/dev/fuse` and `fusermount3`, and re-run their own binary with `#[ignore]`d child tests selected |
| `fsbench` | its benchmark tests mount FUSE file systems |
| `tierstore-testkit` | the tests of its socket directories bind a Unix socket under `TMPDIR` (#170) |
| `tierstore-mount-client` | its lifecycle test binds a Unix socket under `TMPDIR` (#170) |
| `tierstore-compat` | `tests/s3.rs` builds the previous release's S3 client on reqwest's defaults, which (with the rustls this release's client turns on) read the system's CA certificates and refuse to build where there are none, as in a farm action; this release's client reads them only for an `https` endpoint |
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
the job's OIDC token (workload identity federation: no secret is stored) and, as that action
documents, checks that the farm answers and masks its address in the log; it writes the farm's
buckconfig under
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
two actions (with buck2-farm) pinned by commit with no version comment, in this file alone; the
build of `//:ci` through buck2-farm with exactly its three inputs, after the farm check, after
farm-connect, after the build of the action's tool from komira's sources, all at one komira commit;
after the build, in order, the four steps that offer the sync patch (above), each word for word;
and the triggers, one job and one runner. It does not fix the job's other steps, each of which could ask for the token: a change to
them is reviewed like any change to a workflow.

A pull request's build and a build of `main` share the farm's action cache, as every build of
every repository on the farm does: a result one writes, the other reads. The farm's cache is
keyed by the action's digest, so a result is reused only for byte-identical inputs and command.

## A run that hangs on a dead connection to the farm

buck2 on its own does not notice a connection to the farm that died without being closed: it keeps
waiting on the open stream for an answer that never comes, and its `Waiting on ...` line goes on
every few seconds whether the action runs or is lost. So the build runs through komira's
buck2-farm action (`uses: komira-ai/komira/.github/actions/buck2-farm@<commit>`, komira-ai/komira#2439),
pinned to the same komira commit as farm-connect. While buck2 runs, the action's watchdog reads the
runner's TCP connections to the farm from the kernel once a second; when one has heard nothing from
the farm for 5 minutes (a live farm sends something at least every 30 seconds, however long the
action), it ends buck2 and its daemon and runs the build again once the farm's port answers. A
build that fails on a cut connection is run again the same way. A step runs at most three times; a
build that fails for its own reasons is never run again, and a build that is slow but whose
connection is alive is never ended. It never reads the log and never bounds how long an action
runs.

What a contributor sees in the step's log when the watchdog ends a run (the port is the runner's
own; the farm's address is masked):

    buck2-farm: watchdog: the connection to the farm from local port 41022 has heard nothing from the farm for 5 min 1 s, past the bound of 5 min 0 s: the farm has closed it and buck2 would wait on it for ever; ending buck2 (SIGINT)
    buck2-farm: stopping buck2's daemon, which holds the dead connection (`<runner temp>/bin/buck2 kill`)
    buck2-farm: the first run hung on a dead connection to the farm and was ended; waiting for the farm's port to answer, up to 20 min 0 s

then buck2's output of the next run, and one line in the job's summary naming the hang, the wait
and the run (`run 2 of at most 3`). The first run's log is kept on the runner as
`buck2-farm.first.log`. A third run that hangs too fails the step, with
`buck2-farm: FAILED: the third run hung on a dead connection to the farm too: the connection to the
farm was lost in each of 3 runs; not run again`.

The action's tool, `assert_remote`, is built in the job from komira's sources at that commit
(`tools/build/ci/assert_remote/*.zig`, checked out alone, into `target/komira`) with Zig 0.15.2,
fetched by `cargo xtask fetch` once its SHA-256 matches, as komira builds it (`-OReleaseSafe -fstrip`,
a static musl binary). This repository keeps no copy of it. To move to a newer komira, change the
one commit in all three places (farm-connect, buck2-farm, the `ref:` of the sources); the lint
refuses them apart.

## Time limits

Every step of `farm.yml` and of `macos-farm.yml`, the two workflows that reach the build farm, has
a `timeout-minutes` of its own, under its job's, so no step can hang the job to its limit. The
workflow lint holds it (`crates/xtask/tests/workflows/farm_steps.rs`): in any workflow with a step
that uses farm-connect or buck2-farm or runs `cargo xtask mac-farm`, every job with steps has a
whole-number `timeout-minutes` and every step one of its own from 1 and under the job's; and no
workflow runs buck2's `build`, `test`, `bxl` or `run` in a `run:` step of its own.

The bounds come from the longest successful run of each step in the 100 newest successful runs of
`farm.yml` (`gh run list --workflow farm.yml --status success -L 100`, each run's steps from
`gh run view --json jobs`; measured): the build step 45.2 minutes at most (20.8 at the 90th
percentile), farm-connect 1.6, every other step 0.3 or less. `macos-farm.yml` has never run (its
caller in `artifacts.yml` was skipped in all 49 of its runs among the 60 newest successful pushes),
so its steps' bounds come from the same steps elsewhere: checkout and `rustup toolchain install`
0.2 minutes at most, `Compile xtask` (cold, no cache) 1.0 in `artifacts.yml`'s `version` job, and
the farm step keeps its bound.

| workflow | step | longest green run | bound (minutes) |
|---|---|---|---|
| `farm.yml` (job 150) | checkout, `rustup toolchain install`, cache restore and save | 0.2 | 10 |
| | Install buck2 (builds xtask on a cache miss) | 0.3 | 20 |
| | Install Zig, komira's sources, Build assert_remote | new | 10 |
| | Connect to the farm (its own wait for the farm is at most 20) | 1.6 | 30 |
| | `cargo xtask farm-check` | new | 10 |
| | `buck2 build //:ci` through buck2-farm | 45.2 | 120 |
| | Stop buck2 | new | 5 |
| `macos-farm.yml` (job 90) | checkout, `rustup toolchain install`, the tailnet join, the two uploads | 0.2 | 10 |
| | Compile xtask | 1.0 | 20 |
| | `cargo xtask mac-farm` | never run | 75 (unchanged) |

120 for the build leaves room for a run that hangs (5 minutes), the wait for the farm (at most
20) and a run again, beside the longest green run.

What the watchdog does not cover: every step that does not run buck2. `cargo xtask mac-farm`
(`macos-farm.yml`) is its own client of the farm: it follows a cut Execute stream again (at most
three cuts) but has no deadline of its own after its 60-second connect, so a connection that dies
silently there ends at its step's 75 minutes. The tailnet joins, the downloads and the cargo steps
end at their own bounds above. The workflows that do not reach the build farm (`ci.yml`,
`artifacts.yml`, `release.yml` and the rest) are not changed.

## Measured

The first measurements of the lane: `buck2 build //:ci` of this branch (buildrules at a693e5c),
run by hand from a development machine against the farm, one run each, the second and third
immediately after the first. Every action ran on the farm or came from its cache; the machine ran
none. The first wall time is from the run's start and end, the others the shell's.

| build | `Commands:` line | wall |
|---|---|---|
| `//:ci`, after earlier runs of the same day had put most units in the farm's cache | `130 (cached: 9, remote: 121, local: 0)`, BUILD SUCCEEDED | 12 min |
| the same, again, daemon kept | no action looked up | 0.04 s |
| the same, again, buck2's daemon restarted (nothing in its memory) | `2001 (cached: 2001, remote: 0, local: 0)` | 1.6 s |

Every git dependency (hostkeeper's crates, the nfsserve fork, the previous tierstore) is built from
GitHub's archive of its pinned commit, and its units are cached like a crates.io crate's.
