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

The default build produces an optimized, fully static Linux/musl executable.
The subproject pins and provisions its own Rust toolchain (currently 1.96) and
uses `rustup` when available, so it does not depend on Sarun's repository
tooling. Zig and `cargo-zigbuild` must be available:

```sh
make
# target/<architecture>-unknown-linux-musl/release/bumba
```

When `rustup` is present, `make` also installs the selected musl standard
library target if it is missing. `CARGO`, `RUSTC`, `RUSTDOC`,
`RUST_TOOLCHAIN`, and `STATIC_TARGET` remain overridable for packaged
toolchains and cross-build environments.

Development and test builds remain explicit:

```sh
make debug
make test
./target/debug/bumba -c 'printf "hello\\n" | wc -l'
```

Run `bumba` in a terminal for an interactive prompt. With redirected standard
input it behaves as a script interpreter instead. The ordinary shell forms are
available as well:

```sh
bumba --help
bumba -c 'printf "%s\\n" "$1"' command-name argument
bumba script.sh argument
printf 'printf "from stdin\\n"\n' | bumba
```

The executable is multicall by argument zero when installed as `make`,
`gmake`, or `ninja`. The explicit forms `bumba make ...` and
`bumba ninja ...` are also supported.

`BUMBA_TARGET_LOG_DIR=/path` opts Make into per-target captured logs. For
scheduler profiling, `BUMBA_SCHED_STATS=1` reports concurrency, jobserver waits
and wakeups, recursive worker assistance, and elapsed scheduler time on stderr.
Recursive Make output preserves the surrounding Brush redirection or pipeline
directly; capture mode drains concurrently, so output larger than a kernel pipe
cannot make nested builds wait on their own reader.

Pipelines between descriptor-free leaf builtins stay in process memory with
bounded backpressure. Mixed or dynamically resolved pipelines automatically use
kernel pipes where a native descriptor may be required.
