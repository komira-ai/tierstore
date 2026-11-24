"""The two actions of the Zig toolchain measurement (README.md): the writer, whose output directory is the toolchain's tree, and the
reader, whose input is that tree and whose output is its report.

`nonce` is put in the action's environment and nowhere else: a new value is a new action key with the same command and the same
output, so the action runs again instead of being answered from the action cache. Empty, the environment is empty, and the key is
what it is without the attribute.
"""

def _env(name, nonce):
    return {name: nonce} if nonce else {}

def _tree_write_impl(ctx):
    out = ctx.actions.declare_output(ctx.attrs.out, dir = True)
    argv = [ctx.attrs.tool[RunInfo], ctx.attrs.src[DefaultInfo].default_outputs[0], out.as_output()]
    if ctx.attrs.salt:
        argv += ["--salt", ctx.attrs.salt]
    ctx.actions.run(cmd_args(argv), env = _env("TS_ZIG_WRITE_NONCE", ctx.attrs.nonce), category = "ts_zig_tree_write")
    return [DefaultInfo(default_output = out)]

tree_write = rule(
    impl = _tree_write_impl,
    doc = "An action whose output directory `out` is a copy of `src`'s directory, made by `tool` (tree_copy), salted with `salt` when it is not empty.",
    attrs = {
        "nonce": attrs.string(default = ""),
        "out": attrs.string(default = "tree"),
        "salt": attrs.string(default = ""),
        "src": attrs.dep(),
        "tool": attrs.exec_dep(providers = [RunInfo]),
    },
)

def _tree_read_impl(ctx):
    out = ctx.actions.declare_output(ctx.label.name + ".json")
    argv = [
        ctx.attrs.tool[RunInfo],
        "--out",
        out.as_output(),
        "--threads",
        str(ctx.attrs.threads),
        "--passes",
        str(ctx.attrs.passes),
        ctx.attrs.tree[DefaultInfo].default_outputs[0],
    ]
    ctx.actions.run(cmd_args(argv), env = _env("TS_ZIG_READ_NONCE", ctx.attrs.nonce), category = "ts_zig_tree_read")
    return [DefaultInfo(default_output = out)]

tree_read = rule(
    impl = _tree_read_impl,
    doc = "An action that reads and hashes every file of `tree`'s directory with `tool` (tree_read) and outputs the report.",
    attrs = {
        "nonce": attrs.string(default = ""),
        "passes": attrs.int(default = 2),
        "threads": attrs.int(default = 16),
        "tool": attrs.exec_dep(providers = [RunInfo]),
        "tree": attrs.dep(),
    },
)
