# Working on sarun — read this first

**STOP assuming "deps aren't installed, nothing runs here."** They install
themselves. This repo runs fine in this container. Below is exactly how.

**This entire repo was written by Claude (you) — zero human changes.** So there
is no hidden human setup, no external environment you're missing. Everything
needed to run and test is in-repo plus `uv`. If something looks like it needs a
venv or a manual install, that's a stale artifact a past session left — fix it,
don't work around it. (Example already fixed: `test_e2e.py` once hardcoded a
`/home/user/venv` that never exists here.) Trust the established patterns; don't
re-derive "can it even run" every session — it can.

## What `sarun` is

There are TWO binaries with the name `sarun`:

* **`prototype/sarun`** — the original Python single-file app. The file IS the
  executable: shebang `#!/usr/bin/env -S uv run --script`, PEP 723 deps in a
  `# /// script` block, **uv installs every dependency on first run.** You do
  not pip-install anything, you do not need a venv. The Rust port was developed
  and tested against this prototype, and it still works.
* **`engine/target/.../sarun`** — the Rust port. Same control protocol, full
  standalone UI+engine. Production target.

The prototype + its tests + the `bench/` harness all live under `prototype/`
so a top-level `./sarun` does not exist by accident — see the Makefile for
the top-level entry points. The default for `make run` is the Rust binary if
built, else the prototype.

`sarun` is **filesystem/proc only**: it sandboxes a command over a copy-on-write
overlay of your filesystem, captures its writes/processes/output for review, and
applies/discards them. **Box networking is a per-box choice (engine `NetMode`),
NOT "always host".** The default is `Tap` (proxied): the box gets a per-box
netns wired to a userland TCP/IP stack the engine drives in-process — DHCP,
DNS, an HTTPS MITM proxy that injects its own CA into the box, and a per-flow
policy hook (all under `engine/src/net/`). Opt out with `--net off` (an empty
netns where every dial fails closed) or `-N` / `--net host` (share the host
netns for raw connectivity). `-n` is the explicit spelling of the `Tap`
default; `--net off|tap|host` is the canonical selector. (The Python
**prototype** has none of this — it is host-net-only, no `--unshare-net`; the
proxy stack is engine-only.) The untrusted binary viewer (`run_on_untrusted`,
used to render box-produced bytes) runs under bwrap `--unshare-all`, fully
air-gapped. `sakar` is a separate sibling tool (top level, its own
`test_sakar*.py`) — do NOT touch `sakar` when working on `sarun`.

## Run the app
The Makefile is the entry point. `make` (no args) lists every command.
```
make run                       # Rust binary if built, else prototype/sarun
make run-py                    # always the prototype
make engine                    # build the Rust port (fully-static musl binary)
```
Or invoke directly:
```
prototype/sarun -h
prototype/sarun                       # starts the UI/server
prototype/sarun -- some cmd           # runs `some cmd` in a sandbox
```
The prototype's first run also **builds a patched pyfuse3** (see section 0 of
`prototype/sarun`): downloads a pinned sdist, applies an embedded patch,
compiles it into `~/.cache/sarun/…`. That takes ~25 s ONCE (needs network + a
C toolchain: gcc, pkg-config, libfuse3 dev headers), then it is cached and
every later run is ~0.4 s. Do not be surprised by the first-run pause; it is
not a hang. `make warmup` pays this cost deliberately.

Some fresh containers are missing the system packages. If the pyfuse3 build
fails with `Package 'fuse3' … not found`, or boxes die with
`FileNotFoundError: 'bwrap'`, install them first:
```
apt-get install -y libfuse3-dev fuse3 pkg-config bubblewrap
```

## The Rust port — static musl, the only build
The engine crate lives in `engine/` → one binary at
`engine/target/x86_64-unknown-linux-musl/release/sarun`. The ONLY build is
a fully-static musl binary (the dynamic glibc path is gone — `sarun` ships
a single static executable, and that's what every test harness uses).
Built without `apt`, via `cargo-zigbuild` + `ziglang` from `uv` (a tiny
`musl-gcc → zig cc -target x86_64-linux-musl` shim under
`engine/target/zigshim/` keeps cc-rs happy for the C deps like onig_sys
and rusqlite's bundled SQLite):
```
make engine
file engine/target/x86_64-unknown-linux-musl/release/sarun   # "statically linked"
```
`engine/.cargo/config.toml` sets `build.target = x86_64-unknown-linux-musl`
so plain `cargo build --release` from inside `engine/` also produces the
static binary, AFTER `make engine` has set up the zigshim+ziglang once.
`prototype/test_musl_rs.py` cross-checks the static-linkage guarantee via
`file` + `ldd`.

## Run the tests
The Python prototype's tests, the pytest glue (`conftest.py`), and the `bench/`
harness all live under `prototype/`. Each `test_*.py` is standalone (repo
`check()/_fails` + `__main__` style) AND pytest-compatible; the deps each file
needs are in its module docstring. `sarun` no longer depends on mitmproxy.
```
make test          # the whole suite (excludes test_e2e.py + test_pjdfstest.py)
```
which expands to (in `prototype/`):
```
uv run --with pytest --with pytest-timeout --with "textual>=0.60" \
  --with "wcmatch>=8.4" --with "pyfuse3>=3.2" \
  --with "trio>=0.22" --with "python-magic>=0.4" \
  pytest -q -p no:cacheprovider --ignore=test_e2e.py --ignore=test_pjdfstest.py
```
Expected today: **141 passed**. The `sakar*` tests stay at top level and are
not collected from `prototype/`. A single file:
```
cd prototype && uv run --with pytest --with "pyfuse3>=3.2" --with "trio>=0.22" \
  pytest -q -p no:cacheprovider test_outputs_capture.py
```
(Loading any test imports the prototype, which triggers the section-0 pyfuse3
bootstrap, so the first test run also pays the ~25 s build, then caches.)

These are **real** tests: FUSE actually mounts, bwrap actually runs, the
network actually works in this sandbox. The patched-pyfuse3 assertion means a
test that somehow ran on stock pyfuse3 would fail loudly — by design.

## e2e tests (`prototype/test_e2e.py`)
End-to-end: launches the real headless UI + real `prototype/sarun -- cmd` boxes
(basic, nested, named, forced-userns, stdout/stderr capture). bwrap and FUSE
mounts work in this sandbox — so these really run here. Has a uv shebang:
```
make test-e2e        # or directly: prototype/test_e2e.py
```
Expected: `E2E PASS` (one NOTE-skip: nested-inside-userns isn't exercisable as
root). The test process runs under uv (deps for its in-process SourceFileLoader)
and the box/UI subprocesses reuse that same interpreter via `sys.executable` —
there is NO hardcoded venv. Takes a few minutes (real boxes).

## OCI containers (`sarun oci …`) — status + backlog
`oci load|run|build` live in `engine/src/oci.rs` (Dockerfile parser in
`engine/src/dockerfile.rs`). Working & verified against real images (alpine,
busybox, hello-world): pull/archive/layout → at-rest layer-box stack; run a
container box on an image's top layer with its config (env/cmd/entrypoint/
workdir/user); build a Dockerfile (FROM/RUN/COPY/ADD/ENV/ARG/WORKDIR/USER/CMD/
ENTRYPOINT/LABEL/EXPOSE/VOLUME/SHELL, multi-stage, `-t`). Closed/minimal rootfs
boots too — synthetic mount-target landing pads (`overlay.rs::is_synthetic_
landing`, non-destructive via `chain_dir_has_children`), fd-exec of the inner
shim (`runner.rs`, via `/proc/self/fd/N`), and the host-visibility cwd default
(engine `no_host` ack). Regression net: `prototype/test_oci.py` (`make test-oci`)
synthesizes a scratch oci-archive in-test (rootfs = the static engine binary at
`/bin/sarun`, no registry/network) and asserts load → run → build → run-result,
including closed-rootfs boot and that COPY landed. **Delete a bullet below the
moment it's done.**

- [ ] **Tap default for OCI runs.** Every OCI run so far used `--net off`;
  confirm the proxied `Tap` default works *inside* a container (DNS +
  HTTPS-through-proxy) and fix the closed-rootfs/netns interaction if not.
- [ ] **Layer reuse / image cache for `oci run`.** `resolve_image_top` only
  matches `<ref>` by box name/id, so a repeat `oci run <ref>` re-pulls into a
  fresh stack. Also match the `oci_reference` meta of an already-loaded stack
  and reuse it as the parent — multiple runs then share the layer boxes (the
  Docker model; only the per-run container box is new). Key v1 = ref string;
  v2 = manifest digest (so a moved tag re-pulls, `name:tag`/`@digest` coalesce).
- [ ] **Container GC.** Each run leaves an at-rest container box; add `oci ps`
  / `oci rm` (or prune) once layer reuse lands.
- [ ] **`oci build` instructions:** `COPY --from=<stage|image>`; `ADD` URL
  fetch + local-tar auto-extract; glob sources; carry HEALTHCHECK/ONBUILD/
  STOPSIGNAL into the image config (today: warned + skipped).
- [ ] **Registry reach:** private-registry auth (`fetch_registry` passes
  `RegistryAuth::Anonymous`); digest/signature verification; `zstd:chunked`
  fast path (currently decoded as plain zstd — correct, just not chunked).
- [ ] **`--api` `/usr/local/bin/{oaita,sarun}` on scratch/distroless (low pri,
  maybe wontfix).** These are FUSE shadows (`overlay::is_engine_shadow_path`),
  no binds. On a closed minimal image with no `/usr/local/bin` the box can't
  traverse to the shadow leaf — but **sarun never needs it**: its own in-box
  engine access goes over SCM_RIGHTS (the FD broker / fd-exec'd inner), never a
  path. Those PATH entries exist only so a *user* inside the box can type
  `oaita`/`sarun`. If that's wanted on such images, present the ancestor dirs
  too (as `oaita_config_ancestor_or_self` does for the config path); otherwise
  leave it.
- [ ] **Reconcile opaque-whiteout.** `oci.rs` does `set_opaque` for
  `.wh..wh..opq`, but the file header still calls it "out of scope
  (logged + skipped)" — verify the behavior, fix whichever is wrong.

## Branch / workflow
Develop on the branch you were told to; commit with clear messages; push only
when asked (`git push -u origin <branch>`). One clean commit per logical change.

## The one true rule
Before claiming something can't run, **run it.** `prototype/sarun -h`, `make`,
and `make test` all work from a clean checkout here. If a step seems to need a
venv or a pip install, you are holding it wrong — it's a uv script.
