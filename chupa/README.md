# Chupa

Chupa is Sarun's standalone local-mirror project: it acquires, updates,
publishes, browses, and reads large local collections without depending on
Sarun. The name reflects its intended result—a substantial pile of useful
local data.

The root `chupa` crate owns the durable job supervisor, archive HTTP gateway,
terminal reader, and standalone terminal GUI. The workspace also contains the
storage primitives and drivers for Git, IETF, MediaWiki/Wikipedia, media,
wikitext, and Scribunto.

Sarun depends on Chupa and supplies three narrow adapters:

- its namespaced XDG state root, preserving existing `mirrors.db` jobs;
- captured-site rows from Sarun boxes;
- optional image enhancement for the HTTP gateway.

There is no dependency in the other direction.

## Run

```sh
cargo run --manifest-path chupa/Cargo.toml -- gui
cargo run --manifest-path chupa/Cargo.toml -- list
cargo run --manifest-path chupa/Cargo.toml -- add wiki lvwiki /data/lvwiki.swdump 86400
cargo run --manifest-path chupa/Cargo.toml -- wikimak --help
```

The GUI can register, run, pause, cancel, remove, and read mirrors. Driver
multicall entry points (`wikimak`, `ietfmak`, and `gitdepot`) remain available
both through `chupa DRIVER ...` and through symlinks named after a driver.

Standalone state defaults to `$XDG_STATE_HOME/chupa` (or
`~/.local/state/chupa`). `CHUPA_STATE_HOME` selects an explicit state root.
The archive gateway listens on `127.0.0.1:8642` by default;
`CHUPA_GATEWAY_ADDR` overrides it.

## Verify

```sh
cargo test --manifest-path chupa/Cargo.toml -p chupa
cargo test --manifest-path chupa/Cargo.toml --workspace
```

The large-scale Wikipedia and storage evidence, recovered design history, and
format specifications remain beside their owning crates. [DESIGN.md](DESIGN.md)
records the extracted ownership boundary and compatibility decisions.

## Provenance

The lower-level workspace began as the Rust integration tree recovered from
`github.com/telepancake/gimir`; its historical notes retain that name where it
describes the earlier project or on-disk namespaces. Chupa is the current
product and dependency boundary.
