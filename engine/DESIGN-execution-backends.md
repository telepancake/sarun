# Unified filesystem and execution backends

Status: accepted implementation roadmap, 2026-07-16.  Keep the checklist in
this file current as work lands.  Commit incomplete but valuable milestones;
an honest WIP commit is preferable to losing a long implementation run.

## Invariant

Sarun has one filesystem implementation and three transports:

```
                         sarun overlay/capture policy
                                    |
                    virtiofsd::filesystem::FileSystem
                       /             |              \
              /dev/fuse         SUD shared ring      vhost-user-fs
                 |                   |                    |
              bwrap            SUD processes       QEMU + Linux
```

Path resolution, layer precedence, copy-up, whiteouts, rules, attachments,
capture, and synthetic nodes exist only in the shared filesystem.  A transport
may translate requests, move bytes, and transfer backing file descriptors; it
must not reinterpret filesystem policy.

`tv/sud` remains the Syscall User Dispatch frontend.  What will disappear is
its separate filesystem: `tv/sud/inramfs`, custom overlay/path-remap semantics,
and the engine's matching sweep/reader code.  A SUD filesystem syscall becomes
a canonical FUSE request sent over bounded shared-memory request/completion
rings.  A narrow Unix ancillary-fd lane is permitted only for operations such
as executable mappings and mmap that genuinely need a host fd.

## Fixed decisions

- Upstream filesystem basis: pinned `virtiofsd` 1.14.0.  The vendor patch makes
  daemon-only dependencies optional and replaces the passthrough library's two
  libcap-ng operations (`DAC_OVERRIDE` and `FSETID`) with direct Linux
  `capget`/`capset`, keeping sarun's static-musl library build C-library-free.
- QEMU: pinned 11.0.2.  Linux: pinned 6.18 LTS.  Sources and built artifacts
  live in the external cache; hashes, configs, patches, and reproducible build
  recipes live here.
- CLI: `sarun run --fuse`, `--sud`, or `--qemu ARCH`.  The initial appliance
  architectures are `aarch64` and `x86_64`.
- Matching host/guest uses KVM when available.  TCG is the required fallback,
  including aarch64 on the current aarch64 host without `/dev/kvm`.
- The guest boots directly from tag `sarun-root` with
  `root=sarun-root rootfstype=virtiofs rw`.  Target sarun is `/init`, mounts the
  essential pseudo-filesystems, supervises/reaps the command tree, forwards
  signals, reports status/provenance over virtio-serial, and powers off.
- No live migration in this phase.  Filesystem serialization reports
  unsupported.
- New lifecycle/backend fields use the generated binary transport relation.
  Do not add another legacy JSON representation.
- `BoxState`, sqlar, and the depot remain the persistent representation.
- Unrelated `viros/` work and top-level user files are never touched.

## Work checklist

### 1. Upstream seam and shared core

- [x] Pin/vendor the library and gate daemon-only native dependencies.
- [x] Prove fully static aarch64 and x86_64 musl builds.
- [x] Introduce `SarunFs` implementing `FileSystem` and explicitly unsupported
      `SerializableFileSystem` migration hooks.
- [x] Separate virtual inode/handle lifetime, overlay policy, underlying layer
      access, capture/provenance, and synthetic nodes.
  - [x] Make inode identity, lookup counts, handle allocation/lifetime, node
        kinds, and node attributes transport-independent; fuser and virtio-fs
        only encode canonical values at their reply boundaries.
  - [x] Isolate merged-layer resolution and ordinary backing-file access.
    - [x] Move precedence, whiteout, hole, opacity, rebase, attachment, and
          synthetic-landing decisions into a pure transport-independent layer
          resolver with direct behavior tests.
    - [x] Delegate host-backed lookup, attributes, open/read/release, readlink,
          directory cookies, copy-up reads, and statfs to upstream passthrough.
          Native paths remain only at explicit direct-write and negotiated
          kernel-passthrough acceleration boundaries.
  - [x] Isolate capture/provenance mutation from protocol callbacks.
    - [x] Give filesystem mutations and process-provenance insertions one
          bounded journal owned outside the overlay/protocol adapters.
    - [x] Move depot mutations, copy-up attribution, and finalization behind a
          capture service rather than calling `BoxState` throughout policy.
  - [x] Isolate synthetic projections, sinks, jobserver, and nested-box nodes.
    - [x] Define every reserved per-box name, kind, attribute, and sink stream
          once in a typed transport-independent synthetic-node catalog.
    - [x] Move synthetic runtime behavior and projected-file ownership out of
          the merged overlay implementation.
- [x] Use virtiofsd passthrough machinery for ordinary host-backed operations;
      retain only sarun-specific composition and capture policy.
- [ ] Add canonical-message tests for lookup counts, forgotten/open inodes,
      rename/unlink lifetime, hardlinks, symlinks, xattrs, ownership, locks,
      mmap, sparse files, truncation, directory cookies, concurrency, and bad
      requests.

### 2. FUSE cutover

- [x] Add a thin `/dev/fuse` transport into the virtiofsd server/SarunFs.
  - [x] Feed ordinary FUSE request/reply buffers through the same virtiofsd
        opcode decoder used by vhost-user; retain transport-specific byte
        movement only at the reader/writer boundary.
  - [x] Own the mount/device worker lifecycle and dispatch `/dev/fuse` buffers
        through that decoder.
- [ ] Preserve the synthetic box-id root, sinks, jobserver, shadows, rules,
      attachments, nested boxes, live events, and passthrough behavior.
  - [x] Keep backing-file eligibility in `SarunFs` while the raw transport owns
        backing-open/close ioctls and reply IDs; fall back once on kernels or
        unprivileged daemons that reject registration (Linux currently requires
        `CAP_SYS_ADMIN`), matching the old adapter's behavior.
  - [x] Run the live engine on the raw transport and prove captured write/read
        without host escape, stdout sink delivery, blocking host jobserver
        acquire/release, UI materialization, and live overlay events on aarch64.
- [ ] Differential-test old and new implementations on identical workloads.
- [ ] Cut over, delete the old `fuser::Filesystem` implementation, and remove
      `fuser`.

### 3. SUD cutover

- [ ] Implement bounded multi-producer shared request/completion rings with
      futex/eventfd wakeups and crash-safe cancellation.
- [ ] Translate intercepted syscalls and fd operations to canonical FUSE
      requests without resolving paths or applying overlay policy in SUD.
- [ ] Add the exceptional SCM_RIGHTS fd lane for exec/mmap backing objects.
- [ ] Retain trace/provenance independently of the filesystem transport.
- [ ] Reach FUSE/SUD equivalence for visible trees, metadata, sqlar, output,
      provenance, networking, nesting, OCI, brush, and termination.
- [ ] Delete inramfs, SUD overlay/path-remap semantics, `sudir.rs`, upper-dir
      sweep logic, and their wire/runtime compatibility fields.  Do not delete
      the SUD dispatcher/frontend itself.

### 4. QEMU appliances

- [x] Extend the transport relation with backend `qemu` and required
      architecture; reject architecture on non-QEMU registrations.
- [x] Add verified, cached QEMU/Linux source builders and per-architecture
      manifests/configs.
- [x] Build minimal paired QEMU/kernel artifacts with virtio-fs, console,
      virtio-serial control, KVM, and TCG support required by sarun.
- [x] Add the host launcher/vhost-user backend and target `/init` control plane.
  - [x] Embedded vhost-user lifecycle serves a scoped `SarunFs` box root on a
        private per-box socket and exits when its frontend disconnects.
  - [x] QEMU registration uses the generated binary request/reply directly;
        its architecture and virtio-fs socket never acquire a JSON form.
  - [x] Launch the paired QEMU appliance and implement the guest `/init`
        control endpoint.
- [x] Connect off/host/tap networking with the intended policy boundaries.
  - [x] For `tap`, carry one Ethernet frame per datagram between virtio-net and
        the existing per-box smoltcp stack; keep QEMU out of Prolog and out of
        the network-policy implementation.
  - [x] Prove `off` has no usable network, `host` uses QEMU user networking for
        unrestricted TCP/UDP without policy/capture, and `tap` resolves and
        forwards TCP through the ordinary rules/MITM/capture dispatcher.
  - [x] Stop packet stacks and dispatcher threads deterministically when the
        box channel closes; surface setup failure as registration failure
        instead of returning a box with dead networking.
  - [x] Include the small Linux userspace ABI needed by real tools (futex,
        eventfd, epoll, timers, locks, rseq, memfd, inotify) while explicitly
        excluding default-y device families absent from the paired machines.
- [x] Boot aarch64 under TCG on the current host; verify a successful command,
      non-zero exit propagation, captured filesystem writes, and no host write.
- [x] Boot x86_64 under TCG on the aarch64 host and execute the projected
      x86_64 `/init` through the same binary control and SarunFs transport.
- [ ] Pass the full appliance suite on aarch64 TCG here, then aarch64 KVM where
      available, then x86_64 TCG/KVM.

### 5. Final equivalence and deletion

- [ ] Exercise git, SQLite, Cargo, GNU make/Kati, Ninja, Autoconf, CMake,
      archive extraction, parallel compilers, and execution of new binaries on
      every backend.
- [ ] Stress concurrency and forced termination; prove no write escapes to the
      host and no ring waiter remains stuck.
- [ ] Record comparable filesystem benchmarks.  Do not delete a displaced
      backend until its replacement meets or beats its benchmark geometric
      mean.
- [ ] Remove backend-specific semantic branches and obsolete compatibility
      code; update generated help and user documentation.

## Commit gates

Commit and push after: this roadmap; static upstream seam; shared-core protocol
tests; FUSE parity/cutover; SUD ring; SUD parity/deletion; reproducible appliance
builders; aarch64 appliance; x86_64 appliance; final equivalence and cleanup.
During any long gate, commit and push a compiling WIP checkpoint rather than
holding hours of work only in the worktree.

## Known baseline failures

- The 2026-07-17 full static aarch64 unit run passed 377 tests, ignored one,
  and exposed two pre-existing Brush/editor semantic-completion assertions:
  `production_brush_document_propagates_later_find_type_constraint` and
  `bash_editor_uses_relation_for_backward_completion_and_insertion`.  The
  QEMU appliance, generated-wire, and backend lifecycle subsets are green.
  These failures require finishing the declarative `find` argument grammar;
  they are not hidden by filesystem-backend test runs.
