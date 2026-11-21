# Written once by `buildrules sync`; yours after that. The workspace's targets: :check, and one aggregate per step.
load("//:buildrules.bzl", "cargo_workspace")

cargo_workspace(path = ".")

# The files of the repository outside the crates that a crate's code or tests read: an `include_str!`
# or `#[path]` at compile time, or a file a test opens. A Buck2 action sees only its declared inputs,
# so each member that reads one names it in `extra_srcs` (its BUCK file). Each is exported by
# reference: the file stays at its path in the repository, so the relative path the code uses
# (`../../packaging/...`) resolves in the action as in a checkout.
_READ_BY_CRATES = [
    "LICENSE",
    "docs/design/fetch-cache-serve.md",
    "docs/design/tierstore-handover.md",
    "docs/measurements",
    "packaging",
]

[
    export_file(
        name = path,
        src = path,
        mode = "reference",
        visibility = ["PUBLIC"],
    )
    for path in _READ_BY_CRATES
]
