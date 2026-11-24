"""The two actions of the Zig toolchain measurement (README.md): the writer, whose output directory is the toolchain's tree, and the
reader, whose input is that tree and whose output is its report.

`nonce` is put in the action's environment and nowhere else: a new value is a new action key with the same command and the same
output, so the action runs again instead of being answered from the action cache. Empty, the environment is empty, and the key is
what it is without the attribute. `write_action` and `read_action` make each action's command, environment and outputs; the rules
and `nonce_cases` (the test of that promise) both call them.
"""

WRITE_NONCE_ENV = "TS_ZIG_WRITE_NONCE"
READ_NONCE_ENV = "TS_ZIG_READ_NONCE"

def _env(name, nonce):
    return {name: nonce} if nonce else {}

def write_action(tool, src, out, out_name, salt, nonce):
    """The writer's action: `argv` (the tool, the source directory, the output directory, the salt if any), `env` and the names
    of its `outputs`. `out` is what the command names the output by; `out_name` is the output's declared name."""
    argv = [tool, src, out]
    if salt:
        argv += ["--salt", salt]
    return struct(argv = argv, env = _env(WRITE_NONCE_ENV, nonce), outputs = [out_name])

def read_action(tool, tree, out, out_name, threads, passes, nonce):
    """The reader's action: `argv`, `env` and the names of its `outputs` (the report)."""
    argv = [tool, "--out", out, "--threads", str(threads), "--passes", str(passes), tree]
    return struct(argv = argv, env = _env(READ_NONCE_ENV, nonce), outputs = [out_name])

def _tree_write_impl(ctx):
    out = ctx.actions.declare_output(ctx.attrs.out, dir = True)
    a = write_action(ctx.attrs.tool[RunInfo], ctx.attrs.src[DefaultInfo].default_outputs[0], out.as_output(), ctx.attrs.out, ctx.attrs.salt, ctx.attrs.nonce)
    ctx.actions.run(cmd_args(a.argv), env = a.env, category = "ts_zig_tree_write")
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
    name = ctx.label.name + ".json"
    out = ctx.actions.declare_output(name)
    a = read_action(ctx.attrs.tool[RunInfo], ctx.attrs.tree[DefaultInfo].default_outputs[0], out.as_output(), name, ctx.attrs.threads, ctx.attrs.passes, ctx.attrs.nonce)
    ctx.actions.run(cmd_args(a.argv), env = a.env, category = "ts_zig_tree_read")
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

def _write(salt, nonce):
    return write_action("TOOL", "SRC", "OUT", "zig", salt, nonce)

def _read(nonce):
    return read_action("TOOL", "TREE", "OUT", "r.json", 16, 2, nonce)

def nonce_failures(write = _write, read = _read):
    """What breaks the nonce's promise in the actions `write(salt, nonce)` and `read(nonce)` make: for each, the action without a
    nonce, and with two different ones, must have the same command and outputs, an empty environment without a nonce, and an
    environment holding exactly the nonce, which differs between the two, otherwise. A REAPI action key is the digest of the
    command (its arguments, environment and output paths) and the input root, so this is a key that changes, and only by the
    environment. Empty: every case holds."""
    failures = []
    cases = [
        ("writer", WRITE_NONCE_ENV, lambda n: write("", n)),
        ("salted writer", WRITE_NONCE_ENV, lambda n: write("s", n)),
        ("reader", READ_NONCE_ENV, read),
    ]
    for what, var, make in cases:
        none, one, two = make(""), make("n1"), make("n2")
        if none.argv != one.argv or one.argv != two.argv:
            failures.append("{}: the nonce changes the command: {} {} {}".format(what, none.argv, one.argv, two.argv))
        if none.outputs != one.outputs or one.outputs != two.outputs:
            failures.append("{}: the nonce changes the outputs: {} {} {}".format(what, none.outputs, one.outputs, two.outputs))
        if none.env != {}:
            failures.append("{}: without a nonce the environment is {}, not empty".format(what, none.env))
        if one.env != {var: "n1"} or two.env != {var: "n2"}:
            failures.append("{}: the environment is {} and {}, not the nonce alone in {}".format(what, one.env, two.env, var))
    return failures

def _broken_write(salt, nonce):
    a = _write(salt, nonce)
    return struct(argv = a.argv + (["--nonce", nonce] if nonce else []), env = {}, outputs = a.outputs)

def _broken_read(nonce):
    a = _read(nonce)
    return struct(argv = a.argv, env = a.env, outputs = a.outputs + ([nonce] if nonce else []))

def _nonce_cases_impl(ctx):
    failures = nonce_failures()
    if failures:
        fail("the nonce of the measurement's actions:\n" + "\n".join(failures))
    # The check can see a defect: a writer that puts the nonce in its command and not its environment, and a reader whose outputs
    # change with it, are each found.
    broken = nonce_failures(write = _broken_write, read = _broken_read)
    for want in ["writer: the nonce changes the command", "writer: the environment is", "salted writer: the nonce changes the command", "reader: the nonce changes the outputs"]:
        if not [f for f in broken if f.startswith(want)]:
            fail("the nonce check did not find a defect it must find: " + want)
    out = ctx.actions.write(ctx.label.name + ".passed", "passed\n")
    return [DefaultInfo(default_output = out)]

nonce_cases = rule(
    impl = _nonce_cases_impl,
    doc = "Analysis fails unless `write_action` and `read_action` keep the nonce in the environment alone (`nonce_failures`), and the check finds the defects it is shown. Its output, `passed`, is a unit test of both programs, so neither is built without it.",
    attrs = {},
)
