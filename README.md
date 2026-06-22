# sarun

Controlled execution for Linux: run a normal program directly on your machine,
but capture everything it writes for review before any of it touches the host.
Not a container — the program runs as you, against your real filesystem, only
through a copy-on-write overlay.

## Commands

- `sarun` — start the engine (if needed) and the interactive UI.
- `sarun run -- <cmd>` — run `<cmd>` in a sandbox box (needs a running
  engine/UI; fails fast otherwise). Flags select networking (`-n`/`-N`/`--net`),
  capture mode (`-t`/`-d`/`-e`), the brush shell (`-b`), a PTY (`-p`), and more.
- `sarun <NAME> apply|discard|rename NEW|patch` — operate on a box from the CLI
  instead of the UI.
- `sarun oci load|run|build|save|dockerfile|author …` — work with OCI images
  (pull/unpack, run a container box, build a Dockerfile, commit a box back).
- `sarun oaita gen|run|call|tail|add|where NAME` — a resumable
  OpenAI-compatible chat/agent runner; inside an `--api` box it reaches the model
  through the engine's proxy, which holds the API key host-side so the box never
  sees it. (Also reachable as an `oaita` symlink.)

Run `sarun -h` for the full surface.

## How a box runs

`bwrap` runs the command with the whole filesystem presented as a copy-on-write
overlay, so every file the program writes lands in that overlay's upper layer
instead of on the host. `/proc`, `/dev`, and similar are mounted normally by
bwrap, not overlaid.

The UI owns a single pyfuse3 mount (created via the setuid `fusermount3` helper
— no root, no user namespace, no fd-passing). It is multiplexed: each running
box is a subfolder `<mnt>/<sid>` presenting `lower=/` merged with that box's
private upper (`live/<sid>/up` + `live/<sid>/index.db` under the runtime dir).
The runner bwraps `<mnt>/<sid>` as `/`; every other bwrap flag (own
pid/ipc/uts namespace, runs as you) is unchanged. Because the UI sits in the
I/O path, the Changes view updates live as the box writes, each write goes
through a policy hook, and writer provenance (pid/exe/argv) is recorded per
change.

bwrap also blocks the box from escalating privilege — `sudo`, writes to
privileged sockets, and similar are denied.

## Networking

Networking is a per-box choice (engine `NetMode`):

- `-n` / `--net tap` (default): a per-box network namespace wired to a userland
  TCP/IP stack the engine drives in-process — DHCP, DNS, an HTTPS MITM proxy
  that injects its own CA into the box, and a per-flow policy hook
  (`engine/src/net/`).
- `--net off`: an empty namespace where every dial fails closed.
- `-N` / `--net host`: share the host network namespace for raw connectivity.

The untrusted binary viewer that renders box-produced bytes for the diff/preview
panes always runs under bwrap `--unshare-all` — no network at all.

## Review and merge

While a box runs its upper lives under the runtime dir (`live/<sid>`). On exit
it consolidates into a patch (text changes) plus a sqlar archive (binary files,
symlinks, and deletions, with provenance) under the state dir, browsable in the
UI alongside every other session. From there you inspect the changes and merge
all, merge some, edit before merging, or discard. Deletions are tracked in the
per-session index, not as on-disk markers; textual files get an automatic diff
view.

While a box is still running the UI shows its process tree (htop-style), lets
you poke processes, and can destroy the whole session. All box processes run as
the user that started the box.

## sakar

`sakar` is a separate sibling tool for standalone network interception. It is
not part of sarun.
