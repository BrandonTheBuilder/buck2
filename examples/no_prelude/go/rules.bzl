load("//toolchain.bzl", "GoCompilerInfo")

def _go_binary_impl(ctx: "context") -> ["provider"]:
    sources = ctx.attrs.srcs
    out = ctx.actions.declare_output("main")

    cmd = cmd_args([ctx.attrs.toolchain[GoCompilerInfo].compiler_path, "build", "-o", out.as_output()] + sources)

    ctx.actions.run(cmd, category = "compile")

    return [
        DefaultInfo(default_outputs = [out]),
        RunInfo(args = cmd_args(out)),
    ]

go_binary = rule(
    impl = _go_binary_impl,
    attrs = {
        "deps": attrs.list(attrs.dep()),
        "srcs": attrs.list(attrs.source()),
        "toolchain": attrs.dep(),
    },
)
