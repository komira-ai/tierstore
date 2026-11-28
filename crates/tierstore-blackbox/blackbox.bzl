"""`blackbox_driver`: the driver of a `blackbox_test` (buildrules' rules/blackbox) of this crate.

The rule runs its driver as one program with one argument, the root the packages were unpacked onto. This crate's driver,
`tierstore-blackbox`, takes `fake-s3` from beside its own binary under a root (src/driver.rs), so the two binaries of `:bin` are copied into one
directory and the driver's path in it is the program: the fake goes with the driver, and needs no package of its own.
"""

_PROGRAMS = ["tierstore-blackbox", "fake-s3"]

def _impl(ctx):
    exes = {out.basename: out for out in ctx.attrs.bins[DefaultInfo].default_outputs}
    missing = [name for name in _PROGRAMS if name not in exes]
    if missing:
        fail("{}: {} has no binary {} (it has {})".format(ctx.label, ctx.attrs.bins.label, ", ".join(missing), ", ".join(sorted(exes.keys()))))
    bundle = ctx.actions.copied_dir(ctx.label.name, {name: exes[name] for name in _PROGRAMS})
    return [
        DefaultInfo(default_output = bundle),
        RunInfo(args = cmd_args(bundle.project("tierstore-blackbox"), hidden = bundle)),
    ]

blackbox_driver = rule(
    impl = _impl,
    attrs = {
        # The member's binaries (`cargo_package`'s `:bin`): `tierstore-blackbox` and `fake-s3` among them.
        "bins": attrs.dep(),
    },
)
