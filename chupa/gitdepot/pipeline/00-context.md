# Context (read by every phase)

- `gitdepot/DESIGN.md` is the design. It is authoritative. Do not paraphrase it
  back; read it and work from it.
- Build (static musl — plain `cargo build` does not work here; see `CLAUDE.md`):
  `cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang cargo zigbuild --release -p gitdepot --tests --target x86_64-unknown-linux-musl`
- Lib + integration test binaries land under
  `target/x86_64-unknown-linux-musl/release/deps/gitdepot-*`.
- Relevant code: `gitdepot/src/` (notably `store.rs`, `lib.rs`, `oidenc.rs`,
  `lanestore.rs`, `reslot.rs`, `variants.rs`, `layer.rs`, `gitsrc.rs`,
  `frame.rs`, `unionstore.rs`, `lanes.rs`, `reflog.rs`, `shards.rs`),
  `depot/`, `depot-vbf/`.
- Some parts of the design are currently implemented in more than one place in
  the tree; which code conforms to the design is something to determine against
  `DESIGN.md`, not to assume.
