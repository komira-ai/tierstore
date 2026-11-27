# Compiler cache: sccache on tierstore

[sccache](https://github.com/mozilla/sccache) caches the output of a compiler. With its S3
backend the cache is a bucket, which every CI runner shares, so a runner that starts with an
empty disk still finds the objects another one compiled. This example points sccache at
tierstore's S3 front and builds a small C project three times. What it shows: the second build
compiles nothing; the objects are in the store; and a fresh sccache over a warm tierstore cache
reads every object from tierstore's disk, with no request to the tier behind it.

It runs in CI as a test of the workspace (`crates/tierstore/tests/examples`), with sccache
0.19.0 fetched by its version and SHA-256. Nothing is in a cloud: both of tierstore's tiers are
directories of the run.

## Run it

You need the `tierstore` binary, sccache 0.19.0 on `PATH`, `make` and a C compiler `cc`. From the
repository:

```sh
cargo run -p tierstore-examples -- run examples/build-cache \
  --tierstore "$(command -v tierstore)" --work /path/to/an/empty/directory
```

The runner checks that this page shows each file of the example as it is, starts tierstore on a
free port with `tierstore.toml`, checks sccache's version, writes the project, runs the three
builds with a reading of tierstore's `/metricz` before and after each, evaluates the checks and
compares their lines with `expected.txt`. It prints the report and exits 0 when they are the
same, and prints what differs and exits 1 when they are not. Each command runs with an
environment of its own: `PATH`, a `HOME` and a `TMPDIR` in the work directory, `LC_ALL=C`, and
what `example.toml` sets; nothing else of yours reaches sccache. The logs are in `logs/` of the
work directory.

## The project

Thirty C files made from one template (`${n}` is 1 to 30), and a `Makefile` that compiles each
with its own call of `$(CC)`, so that sccache looks up each object on its own. sccache caches one
compiler call, so thirty calls are thirty lookups; a single call that compiled all thirty files
would be one call that sccache does not cache.

```c file=unit.c.in
/* Unit ${n} of the generated project: a few lines for the compiler, different in each unit. */
int unit_${n}(int x)
{
    int sum = 0;
    for (int i = 0; i < x; i++) {
        sum += i * ${n} + (x ^ ${n});
    }
    return sum;
}
```

```make file=Makefile
# The generated project: one object for each src/*.c, each compiled by its own call of $(CC),
# so that sccache looks each one up in its cache. OUT is the directory of the objects.
OUT ?= out
SRCS := $(sort $(wildcard src/*.c))
OBJS := $(patsubst src/%.c,$(OUT)/%.o,$(SRCS))

all: $(OBJS)

$(OUT)/%.o: src/%.c | $(OUT)
	$(CC) -O2 -c $< -o $@

$(OUT):
	mkdir -p $@
```

## tierstore

A node with a disk cache in front of a cold tier and no hot tier (see
[the configuration guide](../../crates/tierstore/docs/configure.md#a-node-that-owns-the-data-with-a-cache-and-the-cold-tier)).
A put is written to the cold tier before it is answered and, under the prefix that the policy
keeps, to the cache too, so the next read of the object is a hit. `127.0.0.1:0` is a free port:
tierstore prints the address it bound, which the runner reads.

```toml file=tierstore.toml
# tierstore for the compiler-cache example: one process, no hot tier, a disk cache in front of a
# cold tier. Both are directories in the run's work directory; nothing is in a cloud.
# `${work}` is the work directory, which the runner fills in.

[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "example-build-cache"
bucket          = "examples"
access_key_file = "${work}/access.key"
drain_lb_wait   = "1s"

# Every put goes to the cold tier before it is answered, and a put under a kept prefix is
# kept in the cache too, so the next read of it is a local hit.
[local_cache]
path          = "${work}/cache"
budget        = "1GiB"
write_through = true

[cold_store]
kind = "dir"
path = "${work}/cold"

# The keys the cache keeps: sccache's, under its SCCACHE_S3_KEY_PREFIX.
[[policy]]
prefix = "cache/"
cache  = true
```

## The builds and the checks

sccache keeps no cache of its own here: with the S3 backend it has no local disk cache, the
preprocessor cache (on by default, in `SCCACHE_DIR`) is off (`SCCACHE_DIRECT=false`), and every
build starts a new sccache server with an empty `SCCACHE_DIR`. So a hit of the second and third
builds is an object read from the bucket, and a check of tierstore's counters is a check of
tierstore, not of sccache.

The checks of tierstore count over one step: `tierstore_cache_hits_total` (a read answered from
the cache's disk), `tierstore_cache_misses_total` (a read of a kept key that was not), and
`tierstore_cache_upstream_reads_total` and `tierstore_cache_upstream_bytes_read_total` (the
requests and bytes the cache sent to the cold tier). A hit count of 31 is the 30 objects and
`.sccache_check`, which sccache reads when its server starts.

```toml file=example.toml
# The compiler-cache example: sccache with its S3 backend on tierstore's S3 front builds a
# generated C project three times.
#
# - build1 starts with nothing cached: every unit is compiled and its object put.
# - build2 starts sccache afresh (a new server, an empty SCCACHE_DIR, no local preprocessor
#   cache) in read-only mode: every lookup is a hit, and tierstore answers every read from its
#   disk cache, with no request to the cold tier.
# - build3 starts sccache afresh in read-write mode, as a CI job does: its start writes
#   .sccache_check again, which tierstore refuses (komira-ai/tierstore#599).

[client]
program      = "sccache"
version_args = ["--version"]
version      = "sccache 0.19.0"

# 30 units, unit_1.c to unit_30.c.
[[generate]]
template = "unit.c.in"
path     = "project/src/unit_${n}.c"
count    = 30

[env]
SCCACHE_BUCKET        = "${bucket}"
SCCACHE_ENDPOINT      = "${endpoint}"
SCCACHE_REGION        = "us-east-1"
SCCACHE_S3_USE_SSL    = "false"
SCCACHE_S3_KEY_PREFIX = "sccache"
SCCACHE_DIRECT        = "false"
SCCACHE_SERVER_PORT   = "${port:sccache}"
AWS_ACCESS_KEY_ID     = "${access_key_id}"
AWS_SECRET_ACCESS_KEY = "${secret_access_key}"
AWS_EC2_METADATA_DISABLED = "true"

[[step]]
name     = "build1"
env      = { SCCACHE_DIR = "${work}/sccache-1", SCCACHE_ERROR_LOG = "${work}/logs/sccache-1.log" }
commands = [
  { argv = ["sccache", "--start-server"] },
  { argv = ["make", "-j4", "-C", "${work}/project", "-f", "${example}/Makefile", "OUT=build1", "CC=sccache cc"] },
  { argv = ["sccache", "--show-stats", "--stats-format", "json"], stdout = "build1-stats.json" },
  { argv = ["sccache", "--stop-server"] },
]

[[step]]
name     = "build2"
env      = { SCCACHE_DIR = "${work}/sccache-2", SCCACHE_ERROR_LOG = "${work}/logs/sccache-2.log", SCCACHE_S3_RW_MODE = "READ_ONLY" }
commands = [
  { argv = ["sccache", "--start-server"] },
  { argv = ["make", "-j4", "-C", "${work}/project", "-f", "${example}/Makefile", "OUT=build2", "CC=sccache cc"] },
  { argv = ["sccache", "--show-stats", "--stats-format", "json"], stdout = "build2-stats.json" },
  { argv = ["sccache", "--stop-server"] },
]

[[step]]
name     = "build3"
env      = { SCCACHE_DIR = "${work}/sccache-3", SCCACHE_ERROR_LOG = "${work}/logs/sccache-3.log" }
commands = [
  { argv = ["sccache", "--start-server"] },
  { argv = ["make", "-j4", "-C", "${work}/project", "-f", "${example}/Makefile", "OUT=build3", "CC=sccache cc"] },
  { argv = ["sccache", "--show-stats", "--stats-format", "json"], stdout = "build3-stats.json" },
  { argv = ["sccache", "--stop-server"] },
]

# A server a failed step left running.
[[cleanup]]
argv = ["sccache", "--stop-server"]

[[check]]
name = "build1.sccache.requests"
json = { file = "build1-stats.json", pointer = "/stats/compile_requests" }

[[check]]
name = "build1.sccache.misses"
json = { file = "build1-stats.json", pointer = "/stats/cache_misses/counts/C", default = "0" }

[[check]]
name = "build1.sccache.writes"
json = { file = "build1-stats.json", pointer = "/stats/cache_writes" }

[[check]]
name = "build1.tierstore.puts"
metric = { series = "tierstore_cache_upstream_puts_total", step = "build1" }

[[check]]
name = "build2.sccache.hits"
json = { file = "build2-stats.json", pointer = "/stats/cache_hits/counts/C", default = "0" }

[[check]]
name = "build2.sccache.misses"
json = { file = "build2-stats.json", pointer = "/stats/cache_misses/counts/C", default = "0" }

[[check]]
name = "build2.objects"
same = { a = "project/build1", b = "project/build2" }

[[check]]
name = "build2.tierstore.hits"
metric = { series = "tierstore_cache_hits_total", step = "build2" }

[[check]]
name = "build2.tierstore.misses"
metric = { series = "tierstore_cache_misses_total", step = "build2" }

[[check]]
name = "build2.tierstore.upstream_gets"
metric = { series = "tierstore_cache_upstream_reads_total{op=\"get\"}", step = "build2" }

[[check]]
name = "build2.tierstore.upstream_heads"
metric = { series = "tierstore_cache_upstream_reads_total{op=\"head\"}", step = "build2" }

[[check]]
name = "build2.tierstore.upstream_bytes"
metric = { series = "tierstore_cache_upstream_bytes_read_total", step = "build2" }

[[check]]
name = "build3.sccache.hits"
json = { file = "build3-stats.json", pointer = "/stats/cache_hits/counts/C", default = "0" }

[[check]]
name = "build3.sccache.misses"
json = { file = "build3-stats.json", pointer = "/stats/cache_misses/counts/C", default = "0" }

[[check]]
name = "build3.tierstore.rewrite_refused"
metric = { series = "tierstore_cache_upstream_put_failures_total{reason=\"exists\"}", step = "build3" }
expected_failure = { issue = "komira-ai/tierstore#599", passing = "0" }

[[check]]
name = "store.objects"
objects = { prefix = "sccache/" }
```

## What it gives

```text file=expected.txt
build1.sccache.requests 30
build1.sccache.misses 30
build1.sccache.writes 30
build1.tierstore.puts 31
build2.sccache.hits 30
build2.sccache.misses 0
build2.objects same
build2.tierstore.hits 31
build2.tierstore.misses 0
build2.tierstore.upstream_gets 0
build2.tierstore.upstream_heads 0
build2.tierstore.upstream_bytes 0
build3.sccache.hits 30
build3.sccache.misses 0
build3.tierstore.rewrite_refused 1 (expected failure: komira-ai/tierstore#599)
store.objects 31
tierstore.stop exit status: 0
```

The second build: every lookup hits (`build2.sccache.hits 30`, no miss), the objects it took
from the cache are those the first build compiled (`build2.objects same`), and tierstore
answered all 31 reads from its disk: no miss and not one request or byte to the cold tier.

A cache that does not keep sccache's keys fails here and nowhere else: sccache still gets its
objects, from the cold tier, so its own lines do not change, but tierstore's hits drop to 0 and
its reads of the cold tier rise. The test runs the example that way too
(`the_build_cache_example_over_a_cache_that_keeps_nothing_differs_at_tierstores_lines`).

## The expected failure

sccache writes `.sccache_check` each time its server starts in read-write mode. The second time,
the key exists, and a `dir` cold tier refuses a plain put of an existing key with 412
(komira-ai/tierstore#599). sccache then prints `storage write check failed` and goes on
read-only: the third build still hits every object, but would write no new one.
`build3.tierstore.rewrite_refused` counts that refused put. When the put replaces the object, the
count is 0, the value the check gives once the issue is done (`passing`), and the run fails
until the check is made an ordinary one.
