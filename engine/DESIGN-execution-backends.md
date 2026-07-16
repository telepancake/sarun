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

- Upstream filesystem basis: pinned `virtiofsd` 1.14.0.  Vendor only the small
  patch needed to make daemon-only cap-ng/seccomp/sandbox dependencies optional
  for sarun's static-musl library build.
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

- [ ] Pin/vendor the library and gate daemon-only native dependencies.
- [ ] Prove fully static aarch64 and x86_64 musl builds.
- [ ] Introduce `SarunFs` implementing `FileSystem` and explicitly unsupported
      `SerializableFileSystem` migration hooks.
- [ ] Separate virtual inode/handle lifetime, overlay policy, underlying layer
      access, capture/provenance, and synthetic nodes.
- [ ] Use virtiofsd passthrough machinery for ordinary host-backed operations;
      retain only sarun-specific composition and capture policy.
- [ ] Add canonical-message tests for lookup counts, forgotten/open inodes,
      rename/unlink lifetime, hardlinks, symlinks, xattrs, ownership, locks,
      mmap, sparse files, truncation, directory cookies, concurrency, and bad
      requests.

### 2. FUSE cutover

- [ ] Add a thin `/dev/fuse` transport into the virtiofsd server/SarunFs.
- [ ] Preserve the synthetic box-id root, sinks, jobserver, shadows, rules,
      attachments, nested boxes, live events, and passthrough behavior.
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

- [ ] Extend the transport relation with backend `qemu` and required
      architecture; reject architecture on non-QEMU registrations.
- [ ] Add verified, cached QEMU/Linux source builders and per-architecture
      manifests/configs.
- [ ] Build minimal paired QEMU/kernel artifacts with virtio-fs, console,
      virtio-serial control, networking, KVM, and TCG support required by sarun.
- [ ] Add the host launcher/vhost-user backend and target `/init` control plane.
- [ ] Connect off/host/tap networking to the existing engine policy.
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
