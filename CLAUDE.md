# Working on sarun

This repo runs and tests in this container with `uv` plus a few apt packages
(below). Python deps install themselves from `prototype/sarun`'s PEP 723 header
on first run — no venv, no pip. If something looks like it needs a venv or a
manual install, that's a stale artifact; fix it.

## What sarun is

sarun runs a command over a copy-on-write overlay of your filesystem, captures
its writes, processes, and output for review, and lets you apply or discard the
result. The command runs as you, under bwrap, against your real filesystem
through the overlay — not a container.

On top of that base it also has:

- **Per-box networking** (engine `NetMode`). Default `Tap` (`-n`): a per-box
  network namespace wired to a userland TCP/IP stack the engine drives
  in-process — DHCP, DNS, an HTTPS MITM proxy that injects its own CA into the
  box, and a per-flow policy hook (`engine/src/net/`). `--net off` is an empty
  namespace where every dial fails closed; `-N` / `--net host` shares the host
  namespace. The untrusted binary viewer (renders box-produced bytes) always
  runs under bwrap `--unshare-all`.
- **OCI** (`oci load|run|build|save|author|dockerfile`, `engine/src/oci.rs`):
  pull/unpack images, run a container box, build a Dockerfile, commit a box back
  to an oci-archive. Pull/unpack run host-side; an in-box `oci build` ships its
  context to the engine's worker. Status and open items are in `engine/DESIGN.md`.
- **oaita** (`engine/src/oaita/`): a resumable OpenAI-compatible chat/agent
  runner. `sarun oaita gen|run|call|tail|add|where NAME` (also reachable as an
  `oaita` symlink). Config is `{config_home}/oaita.toml` (`model`, `base_url`,
  `api_key`). Sessions are folders of turn files under
  `{state_home}/oaita/<name>/`. The upstream key lives only host-side: an `--api`
  box reaches the model through the engine's UDS proxy, which attaches the
  `Bearer` header after the box boundary, and the box's `oaita.toml` is
  FUSE-shadowed to a keyless copy pointed at the in-engine proxy.

## The two `sarun` binaries

- **`engine/target/x86_64-unknown-linux-musl/release/sarun`** — the Rust port.
  The production target: a full standalone UI + engine.
- **`prototype/sarun`** — the original Python single-file app (a `uv` script).
  The Rust port was built against it. It is still load-bearing as the **test
  harness**: every `prototype/test_*.py` imports it (`SourceFileLoader`) for the
  wire-protocol client (`sync_request`), the sqlar readers (`sqlar_content`,
  `sqlar_mode`, …), and the path helpers.

`sakar` (top level, its own `test_sakar*.py`) is a separate sibling tool — do
NOT touch it when working on sarun.

## Build the engine

The only build is a fully-static musl binary, via `cargo-zigbuild` + `ziglang`
from `uv` (no `apt` toolchain). `engine/.cargo/config.toml` sets the musl target
as default, so plain `cargo build --release` inside `engine/` also produces it
once `make engine` has set up the zigshim.

```
make engine
file engine/target/x86_64-unknown-linux-musl/release/sarun   # "statically linked"
```

`prototype/test_musl_rs.py` checks the static-linkage guarantee.

## Run

```
make run        # the Rust binary if built, else prototype/sarun
make run-py     # always the prototype
prototype/sarun -h
prototype/sarun -- some cmd     # run `some cmd` in a sandbox (needs the UI running)
```

The prototype's first run builds a patched pyfuse3 (~25 s, then cached) — needs
network + a C toolchain. `make warmup` pays this once.

## System dependencies

```
apt-get install -y libfuse3-dev fuse3 pkg-config bubblewrap gcc
apt-get install -y iproute2 tshark          # only for the network tests
```

The pyfuse3 build needs `fuse3`/`pkg-config`/`gcc`; boxes need `bwrap`; the
`test_net_rs.py` box datapath needs `ip` (iproute2) and the flow tests need
`tshark`.

## Run the tests

`make test` runs the suite from `prototype/` (real FUSE mounts, real bwrap, real
network) excluding `test_e2e.py`, `test_pjdfstest.py`, and `test_oci.py` (each
has its own target). Build the engine first so the `*_rs.py` tests have a binary
to drive.

```
make test           # the whole suite
make test-e2e       # end-to-end: real UI + real boxes
make test-oci       # hermetic OCI tests (synthetic archive; needs `make engine`)
```

Each `test_*.py` is standalone (`check()`/`_fails` + `__main__`) and
pytest-compatible; `conftest.py` turns the non-raising `check()` idiom into real
pytest failures. The deps each file needs are in its module docstring.

Known-failing today: the brush-coreutil tests (`test_make_rs`, `test_n2_rs`,
`test_brush_link_rs`, `test_brush_nested_sh_rs`) assert that `cp` runs as an
in-process brush builtin, but `cp` has no entry in `brush_gates.rs` (gates
default to forking the host binary; only the utils listed there run in-process).
That gate is unwritten, so those assertions fail — the tests are ahead of the
implementation.

## Vendored, patched upstreams

`uu_cat`, `findutils`, `kati`, `n2`, `brush-*` under `engine/vendor/` are
patched forks (pristine-import commit + a never-squashed patch series, NOT
submodules) so they run as in-process brush builtins. The model, per-crate
provenance, and the update/`rebase --onto` procedure are in
`engine/vendor/README.md`; the how-to-port guide is
`engine/vendor/PORTING-STORY.md`. Read those before touching a vendored crate.

## Branch / workflow

Commit and push are one step here — this is an ephemeral container, so an
unpushed commit is unsynced risk. Push every logical commit immediately
(`git push -u origin <branch>`). One clean commit per logical change.

## The one rule

Before claiming something can't run, run it. `prototype/sarun -h`, `make`, and
`make test` all work from a clean checkout here.
