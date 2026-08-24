# Bumba

Bumba is a single-process Unix build shell assembled from Brush, rkati, n2,
and selected uutils. It can execute ordinary shell scripts, Makefiles, and
Ninja graphs. Shell builtins, selected core utilities, `find`/`xargs`,
launch-state wrappers, Make recipes, and Ninja recipes execute inside the Bumba
process; commands not supplied by Bumba still use normal external execution.
Command discovery retains ordinary Unix behavior (`command -v cat` reports the
executable in `PATH`), but invoking that discovered or otherwise absolute path
is interposed back into Bumba when its basename names a supplied utility.
Nested `sh`, `dash`, and `bash` invocations—including Autoconf's
`exec $CONFIG_SHELL ./configure`—are likewise interpreted by Brush without
replacing or spawning a shell process.

The library is host-neutral. Embedders may install a structured `EventSink`
and a Kati `FileSystemProvider`; the standalone binary uses `tracing` and the
native filesystem. It does not import Sarun types or protocols.

Build and test on Linux:

```sh
make vendor
cargo test --locked
cargo run -- -c 'printf "hello\\n" | wc -l'
```

The executable is multicall by argument zero when installed as `make`,
`gmake`, or `ninja`. The explicit forms `bumba make ...` and
`bumba ninja ...` are also supported.
