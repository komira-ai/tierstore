# tierstore

An S3-compatible object store front with two tiers behind it, and a read cache for build nodes.

tierstore was born in kbf, as `crates/tierstore`, and now lives here. Versions up to 0.2.0 were built in kbf (their
history stays there); from 0.3.0 every release is built and signed in this repository.
Download a release from the [releases page](https://github.com/komira-ai/tierstore/releases) and verify it with
[docs/ci-and-release.md](docs/ci-and-release.md). The move is described in [MIGRATION.md](MIGRATION.md).

The rules every change to this repository is written and reviewed against are in [AGENTS.md](AGENTS.md).
