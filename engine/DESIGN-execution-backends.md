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
      kinds, and node attributes transport-independent; the shared virtio-fs
        decoder alone encodes canonical values at the reply boundary.
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
- [x] Add canonical-message tests for lookup counts, forgotten/open inodes,
      rename/unlink lifetime, hardlinks, symlinks, xattrs, ownership, locks,
      mmap, sparse files, truncation, directory cookies, concurrency, and bad
      requests.
  - [x] Preserve and test open-file-description identity across unlink and
        rename-over. Lazy lower handles become private anonymous copies before
        their name disappears; source handles follow rename, overwritten
        destination handles stay detached, and neither release nor later
        writes can resurrect or rebind a vanished pathname.
  - [x] Give every raw-message frontend the same malformed-request contract.
        Header-bearing decoder failures return `EIO` with the original unique
        ID, and a SUD ring session demonstrably accepts a valid request after a
        malformed one instead of poisoning or cancelling the transport.
  - [x] Exercise xattr size negotiation/removal, uid/gid projection, record
        locks and flush-time release, fsync, sparse seek, truncation, hardlink
        counts, statfs, stable directory cookies, and concurrent canonical
        requests through the public virtiofsd `FileSystem` trait.
  - [x] Implement `O_TRUNC` in the canonical open path and finalize sqlar size
        metadata for path-based truncate. A live QEMU client exposed both gaps:
        raw open flags had left a lower-file tail in place, while both FUSE and
        QEMU had changed the blob length without updating its capture row.

### 2. FUSE cutover

- [x] Add a thin `/dev/fuse` transport into the virtiofsd server/SarunFs.
  - [x] Feed ordinary FUSE request/reply buffers through the same virtiofsd
        opcode decoder used by vhost-user; retain transport-specific byte
        movement only at the reader/writer boundary.
  - [x] Own the mount/device worker lifecycle and dispatch `/dev/fuse` buffers
        through that decoder.
- [x] Preserve the synthetic box-id root, sinks, jobserver, shadows, rules,
      attachments, nested boxes, live events, and passthrough behavior.
  - [x] Keep backing-file eligibility in `SarunFs` while the raw transport owns
        backing-open/close ioctls and reply IDs; fall back once on kernels or
        unprivileged daemons that reject registration (Linux currently requires
        `CAP_SYS_ADMIN`), matching the old adapter's behavior.
  - [x] Run the live engine on the raw transport and prove captured write/read
        without host escape, stdout sink delivery, blocking host jobserver
        acquire/release, UI materialization, and live overlay events on aarch64.
- [x] Differential-test old and new implementations on identical workloads.
  The static aarch64 suite has the identical 377-pass/2-known-failure/1-ignore
  result before and after cutover; live engine capture, sink, jobserver, UI,
  and event workloads also match on the current host.
- [x] Cut over, delete the old `fuser::Filesystem` implementation, and remove
      `fuser`.

### 3. SUD cutover

- [x] Implement the bounded multi-producer shared request/completion transport.
  - [x] Pin one 32/64-bit-stable freestanding ABI: a sealed memfd inherited as
        fd 1021, 32 independently reclaimable request/reply mailboxes, 32 KiB
        raw-FUSE payloads, futex wakeups, explicit shutdown, and dead-owner
        reclamation. Independent slots avoid an enqueue hole when a tracee dies.
  - [x] Pass the ring as the fourth ordered registration fd and own its worker
        lifecycle with the box; feed every request through the same virtiofsd
        decoder and scoped `SarunFs` used by FUSE and QEMU.
  - [x] Add the freestanding SUD client and split large reads/writes so no
        request exceeds the negotiated slot payload.
- [x] Translate intercepted syscalls and fd operations to canonical FUSE
      requests without resolving paths or applying overlay policy in SUD.
  - [x] Implement LOOKUP/GETATTR/OPEN/CREATE/READ/WRITE/FLUSH/RELEASE and a
        virtual-fd table, directory descriptors, symlink-aware traversal,
        link/rename/mutation operations, metadata, sync, and truncate.
  - [x] Route ordinary file-backed mmap and pathname-based ELF inspection/
        loading through exported canonical handles, including writable shared
        copy-up and logical-cwd/dirfd resolution.
  - [x] Add xattr, statfs, locking, sparse/time operations and the remaining
        exceptional exec forms. `execveat(AT_EMPTY_PATH)` exports the already
        open canonical handle and carries that descriptor through the ordinary
        wrapper exec/ELF path; its process-local `/proc/self/fd/N` spelling is
        the only pathname bypass involved.
  - [x] Replace the path-remap and inramfs addins in the production wrapper in
        one cutover; do not retain an old/new runtime mode.
  - [x] Keep process-local `/proc`, `/dev`, and `/sys` at the SUD namespace
        boundary rather than resolving them in the engine process. Remote
        `/proc/self/fd/N` descriptors still resolve to their canonical SarunFs
        handles; local descriptor links and pseudo-filesystem cwd/dirfds stay
        kernel-local. This is fixed transport plumbing, not a policy mode.
- [x] Add the exceptional SCM_RIGHTS fd lane for exec/mmap backing objects.
      It exports only an already-open canonical handle, is serialized across
      forked tracees with dead-thread reclamation, owns deterministic session
      shutdown, preserves writable copy-up/capture, and has behavioral tests
      for the Rust endpoint plus both freestanding x86 ABIs on this aarch64
      host. Path and policy operations remain exclusively in `SarunFs`.
- [x] Retain trace/provenance independently of the filesystem transport. The
      compact trace stream is applied live and finalized automatically when the
      box channel closes; no post-exit filesystem sweep remains.
- [ ] Reach FUSE/SUD equivalence for visible trees, metadata, sqlar, output,
      provenance, networking, nesting, OCI, brush, and termination.
- [x] Delete inramfs and SUD overlay/path-remap sources after the production
      wrapper cutover. `sudir.rs`, upper-dir/lower materialization and sweep
      logic, their generated wire/runtime compatibility fields, the obsolete
      standalone launcher, and the unused fake-exec/cmd-rewrite optional addins
      are gone. The SUD dispatcher/frontend remains and now has exactly two
      fixed adapters: trace and SarunFs.

### 4. QEMU appliances

- [x] Extend the transport relation with backend `qemu` and required
      architecture; reject architecture on non-QEMU registrations.
- [x] Add verified, cached QEMU/Linux source builders and per-architecture
      manifests/configs.
- [x] Build minimal paired QEMU/kernel artifacts with virtio-fs, console,
      virtio-serial control, KVM, and TCG support required by sarun.
- [x] Add the host launcher/vhost-user backend and target `/init` control plane.
  - [x] Embedded vhost-user lifecycle serves a scoped `SarunFs` box root on a
        private per-box engine socket and exits when its frontend disconnects.
        The engine connects that socket itself and transfers the connected
        endpoint to the runner; QEMU consumes an inherited descriptor and
        never resolves engine runtime paths.
  - [x] QEMU registration uses the generated binary request/reply directly;
        its architecture and virtio-fs socket never acquire a JSON form.
  - [x] Launch the paired QEMU appliance and implement the guest `/init`
        control endpoint.
  - [x] Treat guest setup/exec failure as a versioned control result rather
        than a missing reply. PID 1 reports exit 127 after syncing dirty pages;
        the ACPI-free x86 appliance uses its configured triple-fault reboot so
        QEMU exits deterministically under `-no-reboot`.
  - [x] Make the complete appliance boundary descriptor-based. The runner and
        QEMU use a local socketpair for virtio-serial control, while the
        authenticated engine channel transfers the connected virtio-fs fd.
        A FUSE-contained runner can therefore launch a QEMU child without a
        bind-mounted control socket or a namespace-specific pathname mode.
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
- [x] Launch an aarch64 QEMU child from a live FUSE parent through the ordinary
      FD broker. The strict live gate verifies relative naming, recorded
      `parent_box_id`, child-local capture, returned exit/output, and no host
      write; there is no separate nested-appliance path.
- [x] Make runner return a QEMU lifecycle barrier: it half-closes the box
      channel after appliance exit and waits for engine EOF, which means the
      virtio-fs export and live process identity are gone before an immediate
      same-name rerun begins. The engine drops its temporary SCM_RIGHTS source
      fd before joining virtiofsd, so frontend EOF cannot be self-retained.
- [x] Gate exact exit 37, signalled exit 143, versioned exec failure 127,
      environment/cwd transfer, off isolation, host-mode HTTP to a local
      fixture, tap-mode synthetic DNS through smoltcp, same-name stateful rerun,
      and host non-escape on the current aarch64 TCG appliance.
- [x] Carry the existing registration `brush` value through QEMU instead of
      hardcoding it false. Target `/init brush-sh` now runs the ordinary parser,
      and the shared SarunFs shadow projection runs embedded Kati and n2 in a
      live aarch64 TCG guest with results identical to FUSE and no host escape.
- [x] Boot x86_64 under TCG on the aarch64 host and execute the projected
      x86_64 `/init` through the same binary control and SarunFs transport.
      Revalidated after the SUD deletion with target `/init brush`, exact
      captured bytes, and an architecture-mismatched exec returning 127 in
      three seconds instead of hanging until the host timeout.
  - [x] Select brush shadow executables from the box's registered QEMU
        architecture instead of the host process. The automated live gate now
        runs parser-driven brush, target-x86_64 Kati, and target-x86_64 n2;
        projecting the aarch64 host engine into that guest is impossible.
  - [x] Run exact exit 37 and exec-failure 127 cases in the x86_64 guest, then
        immediately rerun one named x86_64 brush box and prove both generations
        of captured state share one archive without a host write. This exercises
        the descriptor teardown barrier under cross-architecture TCG as well.
- [ ] Pass the full appliance suite on aarch64 TCG here, then aarch64 KVM where
      available, then x86_64 TCG/KVM.

### 5. Final equivalence and deletion

- [ ] Exercise git, SQLite, Cargo, GNU make/Kati, Ninja, Autoconf, CMake,
      archive extraction, parallel compilers, and execution of new binaries on
      every backend.
  - [x] Make the real-engine Python suite resolve the current host's static
        musl target instead of hardcoding x86_64 and `/home/user/sarun`.
        On this aarch64 host, the rebuilt static engine serves and captures a
        FUSE box; GNU hello completes archive extraction, Autoconf configure,
        GNU make and execution, and a CMake/Unix Makefiles project compiles and
        executes. The SUD/QEMU and remaining tool/backend legs stay open.
  - [x] Add a portable live backend-equivalence gate. On aarch64, FUSE and the
        paired QEMU/TCG appliance now produce identical captured content and
        metadata for lower copy-up, chmod, hardlinks, nested rename,
        rename-over with an open destination fd, unlink with an open lower fd,
        sparse truncation, execution of a newly created script, tombstones,
        and host non-escape. The gate automatically adds native SUD on x86_64.
  - [x] Extend that gate through parser-driven brush execution and the shared
        make/ninja executable projections. FUSE and aarch64 QEMU/TCG both run
        embedded Kati and n2, capture identical results, and leave the lower
        host project untouched; native SUD joins the same test automatically
        on an x86 Syscall User Dispatch host.
  - [x] Add strict `make test-backend-workloads`: missing tools and failed
        operations are errors, never green skips. Separate attributable boxes
        run Git commits/object storage, SQLite WAL transactions, Cargo/rustc,
        parallel GNU Make/GCC, parallel Ninja/GCC, Autoconf/configure,
        CMake/Ninja, tar extraction/symlinks, and newly built executables.
        FUSE and aarch64 QEMU/TCG produce identical captured results and modes
        while a recursive lower-tree digest proves no host write. The gate
        automatically includes native SUD on x86; that final leg remains open.
  - [x] Preserve request uid/gid when captured files, directories, symlinks,
        and special nodes are created, and project the stored ownership on
        subsequent lookup/getattr. The real Git workload exposed synthetic
        directories appearing as `nobody:nogroup` under unprivileged FUSE;
        canonical tests now use nontrivial uid/gid for all four node families.
- [ ] Stress concurrency and forced termination; prove no write escapes to the
      host and no ring waiter remains stuck.
  - [x] Run five consecutive live FUSE/QEMU equivalence rounds with eight
        concurrent publishers producing 64 rename-published files per backend;
        every barrier, readback, capture comparison, and host non-escape check
        passed. Ring unit tests separately cover concurrent producers,
        dead-owner reclamation, and shutdown releasing clients/servers. Forced
        FUSE and QEMU box process groups are reaped within the bound and a
        subsequent box succeeds through the same engine. Native-SUD soak
        remains open.
- [ ] Record comparable filesystem benchmarks.  Do not delete a displaced
      backend until its replacement meets or beats its benchmark geometric
      mean.
  - [x] Add `make bench-backends`, with timing taken inside each box so boot is
        excluded and with host non-escape checked. Three aarch64-host median
        rounds (2026-07-17) measured: FUSE 90 ms sequential / 6010 ms metadata
        / 4290 ms parallel; aarch64 QEMU under TCG 290 / 27870 / 33620 ms,
        respectively, for a 4.89x geometric ratio to FUSE. This is explicitly
        a TCG result, not KVM. Native SUD and KVM measurements remain open; the
        already-deleted historical SUD filesystem cannot be validly benchmarked
        on this non-x86 Syscall User Dispatch host.
- [x] Remove backend-specific semantic branches and obsolete compatibility
      code; update generated help and user documentation. A repository audit
      finds backend selection only in registration, runner, transport, trace,
      and appliance lifecycle code; filesystem policy remains in `SarunFs`.
      Stale step-number, path-remap, sweep, and default-backend descriptions
      have been corrected while the design documents retain deletion history.

## Validation ledger (2026-07-17, aarch64 host)

- The final static `aarch64-unknown-linux-musl` test harness has 386 passing
  tests and one ignored browser test. Its only two failures are the pre-existing
  relation-completion cases
  `bash_editor_uses_relation_for_backward_completion_and_insertion` and
  `production_brush_document_propagates_later_find_type_constraint`; no
  filesystem, SUD ring, FUSE transport, or QEMU transport test fails.
- The fixed SUD wrapper builds as freestanding static i386 and x86_64 ELF on
  this aarch64 host. All eight 32/64 client, descriptor-lane, canonical-FUSE,
  and VFS fixtures pass, including logical-cwd propagation through local
  pseudo-filesystem transitions.
- A live native-aarch64 FUSE box exits zero and captures the exact requested
  blob without a host write. The final paired aarch64 TCG appliance does the
  same through target `/init brush` and powers down in two seconds.
- The final paired x86_64 TCG appliance runs target `/init brush` on this
  aarch64 host, captures the exact requested blob, and exits through its
  triple-fault reboot in five seconds. An intentionally mismatched aarch64
  `/bin/sh` reports versioned exit 127 and shuts down in three seconds.
- The cross-architecture brush leg is part of `make test-backends`, not a
  manual-only smoke test. Both cached target init binaries are static, and the
  x86_64 guest's make/ninja shadows resolve to the x86_64 init artifact. Exact
  nonzero/exec-failure status and immediate stateful rerun are gated there too.
- Native live SUD remains untestable on this machine: its x86 wrappers require
  an x86 kernel with `PR_SET_SYSCALL_USER_DISPATCH`, while qemu-user/binfmt
  rejects that prctl with `EINVAL`. The 32/64 freestanding behavioral fixtures
  are the strongest valid substitute here; final live parity requires native
  x86 Linux hardware.
- `/dev/kvm` is absent, so the TCG legs are proven here and both KVM legs remain
  open. Native-SUD real projects/soak/benchmarks also remain open; neither
  external-hardware leg is represented as complete from local smoke tests.
- `make test-backends` passes live FUSE/QEMU equivalence on aarch64. The first
  architecture-correct run of the broader historical Python suite collected
  53 tests and produced 10 pass, 1 skip, 42 fail; most failures are explicit
  legacy harness assumptions (`/root` fixtures as uid 501 and default Tap on a
  host whose kernel rejects unprivileged user namespaces), with additional
  stale API/baseline cases. Those are recorded work, not treated as backend
  failures or silently converted to green skips.
- `make test-backends` also launches QEMU from inside a FUSE box through the
  authenticated broker and descriptor-only appliance boundary. It checks the
  persisted parent edge and child archive, not merely a successful boot.
- The same gate runs the aarch64 QEMU lifecycle matrix, including an immediate
  same-name rerun that must observe prior captured state. This regression found
  and now prevents both stale running-box registration and retained frontend-fd
  teardown races.
- `make test-backend-workloads` passes every strict real-tool stage on both
  FUSE and aarch64 QEMU/TCG, compares equal backend observations, and proves
  the caller-writable lower trees remain byte-for-byte and metadata unchanged.

## Commit gates

Commit and push after: this roadmap; static upstream seam; shared-core protocol
tests; FUSE parity/cutover; SUD ring; SUD parity/deletion; reproducible appliance
builders; aarch64 appliance; x86_64 appliance; final equivalence and cleanup.
During any long gate, commit and push a compiling WIP checkpoint rather than
holding hours of work only in the worktree.

## Known baseline failures

- The 2026-07-17 full static aarch64 unit run now passes 386 tests, ignores one,
  and exposed two pre-existing Brush/editor semantic-completion assertions:
  `production_brush_document_propagates_later_find_type_constraint` and
  `bash_editor_uses_relation_for_backward_completion_and_insertion`.  The
  QEMU appliance, generated-wire, and backend lifecycle subsets are green.
  These failures require finishing the declarative `find` argument grammar;
  they are not hidden by filesystem-backend test runs.
