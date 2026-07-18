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
- QEMU and Linux are one tightly coupled Sarun execution appliance, not two
  general-purpose components that happen to interoperate.  Their machine ABI,
  devices, kernel modules, kernel interfaces, boot path, and lifecycle may all
  be changed together when doing so removes translation layers or makes the
  complete system smaller, faster, or easier to reason about.  Upstream
  compatibility outside the paired build is not a constraint.
- CLI: `sarun run --fuse`, `--sud`, or `--qemu ARCH`.  The initial appliance
  architectures are `aarch64` and `x86_64`.
- Matching host/guest uses KVM when available.  TCG is the required fallback,
  including aarch64 on the current aarch64 host without `/dev/kvm`.  Every
  appliance supports both its native 64-bit process ABI and its corresponding
  32-bit ABI (AArch32 on aarch64, i386 on x86_64).  Because AArch32 EL0 is an
  optional ARM CPU feature, an aarch64 host selects KVM only when the host also
  advertises 32-bit process support; otherwise it deliberately retains TCG.
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
- [x] Determine whether the engine-wide FUSE mount can live in a private
      user+mount namespace while ordinary outer runners consume it by path or
      descriptor. It cannot: `/proc/<owner>/root` is subject to ptrace-style
      cross-user-namespace checks, and although an `O_PATH` mount-root fd can
      cross outward with `SCM_RIGHTS` and supports `openat`, an unprivileged
      bwrap user namespace cannot bind a mount owned by its sibling user
      namespace. A standalone child-userns tmpfs fixture reproduces the same
      kernel rejection, so this is not a FUSE or SarunFs defect.
- [x] Remove the engine's outer-namespace FUSE mount using the viable nested
      topology: a small mount-owner/spawn broker creates the user+mount
      namespace, owns the single FUSE mount and launches each FUSE runner as
      its descendant. The runner's bwrap user namespace is then nested below
      the mount owner instead of being its sibling. Preserve direct stdio,
      PTY, signal, pidfd, Tap, nested-run and box-channel behavior through the
      broker, prove all backend/workload gates, and only then remove the
      currently working outer mount. Do not add an alternate mode or retain
      the failed outside-in descriptor path.
  - [x] Use a namespace-fd handoff rather than a command/stdin proxy.  The
        single-threaded top-level FUSE runner joins the broker-owned user then
        mount namespace before Prolog or any other worker starts, and only then
        creates its Tap namespace and invokes bwrap.  The runner consequently
        stays in the caller's process/session/foreground group with its original
        stdio, terminal and signal path, while bwrap's user namespace is a child
        of the mount owner.  A live kernel fixture has proven that this ordered
        `setns(user)`/`setns(mount)` descent works and that a subsequent bwrap
        sees mounts private to the broker; it avoids inventing a terminal and
        signal relay protocol merely to reproduce inherited Unix semantics.
  - [x] Preserve the complete canonical uid/gid space while privatizing the
        mount. A FUSE superblock translates protocol IDs through its creating
        user namespace; creating it in the broker's one-ID namespace makes
        every other owner invalid and Linux rejects writes before FUSE sees
        them. Therefore `fusermount3` creates the superblock in the initial
        user namespace, the broker immediately clones it by unsharing its
        identity-mapped user+mount namespaces, and the outer startup copy is
        removed with a propagating, non-lazy unmount before workers, the
        control accept loop, or any runner become ready. The broker then owns
        the sole live mount, serves authenticated
        namespace-fd handoffs, and unmounts on ordered shutdown or engine EOF.
        Assert the engine/ordinary host mountinfo has no steady-state FUSE
        entry; do not normalize or collapse ownership to fit a one-ID map.
        The aarch64 raw-transport fixture reads through the broker namespace
        while asserting both engine and caller host mountinfo stay clean. The
        portable backend equivalence, 32-bit appliance, nested/flat QEMU,
        live-runner shutdown, Tap capture, and real developer workload gates
        all pass with the broker active; FUSE and QEMU observations match.

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
- [x] Replace generic guest/host emulation at remaining Sarun control
      boundaries with the smallest paired QEMU+kernel ABI.  In particular, do
      not reproduce Unix descriptor passing through a byte-stream mux.  A
      `sarun run --qemu` issued inside a QEMU box is a typed request to its
      still-live host runner.  That runner launches another flat host-side QEMU
      appliance through a broker-authenticated child engine connection and
      relays the nested caller's input, output, signals, and result.  It never
      starts QEMU inside QEMU, and no guest pidfd or host virtio-fs descriptor
      crosses the appliance boundary.
  - [x] Define the nested launch and its input, EOF, signal, output, and result
        as generated appliance-operation frames. Guest PID 1 multiplexes those
        operations on the existing paired control port; the host outer runner
        mints an engine connection over its authenticated box channel and
        inherits that host FD into the flat child launcher. The live aarch64
        gate proves flat host-side appliances, logical parent edges, relayed
        stdin/EOF, output, exact signal status, child-local capture, no host
        write, and parent completion as a child-teardown barrier. PID 1 emits
        a generated `ready` frame after spawning the command, so the host does
        not feed caller input into the serial port while the kernel is still
        booting. A new QEMU device/kernel driver would add more machinery than
        it removes here, so the paired kernel remains free to gain one when a
        later operation actually benefits from it.
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
- [x] Enable and behaviorally gate both process ABIs in each paired kernel.
      The aarch64 kernel carries `CONFIG_COMPAT`/AArch32 ELF support and the
      x86_64 kernel carries `CONFIG_IA32_EMULATION`/i386 ELF support.  Tiny
      freestanding ARM EABI and i386 programs are compiled as real ELF32 files,
      executed through ordinary SarunFs from the host, print a sentinel and
      return exit 32 in both TCG appliances.  The gate also asserts the
      launcher's reported accelerator.  Native aarch64 KVM is conservatively
      disabled when the host lacks AArch32 EL0, preserving the two-ABI promise.
- [x] Pass the full locally runnable appliance suite on aarch64 TCG and the
      cross-architecture lifecycle/build suite on x86_64 TCG.
- [ ] Re-run the same suite with aarch64 and x86_64 KVM where available.

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
- [ ] Gate a from-scratch paired Linux kernel build, its records, and its
      provenance under every runnable backend.
  - [x] Add a reproducible Linux 6.18 workload that configures a fresh output
        tree, runs the real compiler through `make -j10`, measures overlapping
        clang processes, reads Image/vmlinux back out of the archive, compares
        backend artifact hashes, and checks the lower and source trees for host
        writes. FUSE completes with 10 simultaneous clang processes, 813 object
        files, captured Image/vmlinux, 7,994 process rows, recorded output, and
        no host escape.
  - [x] Remove QEMU's hard-coded one-vCPU/256-MiB ceiling. The appliance now
        follows its cgroup/affinity-visible CPU budget (bounded at 16), budgets
        memory per vCPU, and uses multi-threaded TCG when KVM is unavailable.
  - [x] Add a generated binary guest-process event lane from paired PID 1 to
        the engine. Virtio-fs request TIDs are now resolved exclusively through
        that guest process namespace and never through host `/proc`. A live
        QEMU shell/child check records both guest rows, links the child to the
        shell, assigns the shell's row to its created file, and records none of
        the unrelated host services whose numeric PIDs previously collided.
  - [x] Route QEMU command stdout/stderr through the ordinary synthetic
        SarunFs sinks. The engine records each virtio-fs write with its guest
        TID and echoes it over a single host-side box-channel reader that also
        demultiplexes nested-connection descriptors. A live check preserves
        distinct stdout/stderr bytes, records three output rows against the
        guest shell, and leaves the command's stdout free of boot-console text.
  - [x] Exercise a bounded QEMU `make -j10`: thirty one-second recipes finish
        in three waves (seven seconds including TCG boot), all thirty output
        files have guest writers, and the archive contains 67 guest process
        rows. The probe exposed an unset guest wall clock; the paired kernels
        now enable their PL031/CMOS RTC and initialize CLOCK_REALTIME before
        PID 1. Guest and lower-file epoch seconds agree after the rebuild.
  - [x] Finish the QEMU kernel build and byte-for-byte artifact comparison.
        The apparent second stall at 186 objects was the test's shared `flock`
        counter; the final fixture records private, process-free compiler
        intervals and derives overlap from the closed archive. Both backends
        build 823 real objects with ten overlapping clang processes. Their
        4,837,384-byte Image and 6,021,328-byte vmlinux match byte-for-byte
        (SHA-256 respectively `b424e85ff1a243e68ee234c9ea09f97c7109f88bd089d0600d3cfc6d15a98d87`
        and `25655d3f4d8f82b1ac298abd3a20fcf41ad5c72b7e51668db470826a059b6a44`).
        QEMU records 7,916 guest process rows and 57,609 output bytes; FUSE
        records 7,607 and 57,608. Both artifacts have nonzero writer rows and
        neither backend changes the lower or source tree. FUSE takes 151 s
        wall/131 s compile; aarch64 TCG takes 3,624/3,373 s (24.0x wall), which
        is a correctness result rather than an acceptable KVM-performance
        claim.
  - [ ] Run the same from-scratch `make -j10` workload through `sarun run -b`
        under FUSE and QEMU. Require ordinary process/output/artifact checks as
        well as nonempty `brushprov` and `build_edges`, so this validates the
        embedded parser and Kati path rather than merely wrapping a real shell.
    - [x] Pass GNU-make-4 feature detection, preserve top-level `set -e`, give
          snooped shebang interpreters their own argv, and expose inherited and
          explicit exports to parse-time `$(shell ...)` and recursive makes.
    - [x] Preserve recursive command-line-variable precedence across multiple
          make levels. The inherited `MAKEFLAGS` definitions must be applied
          before the child's argv and replaced by variable name; otherwise
          Kbuild's parent `obj=fs` overrides `obj=fs/notify`, recursively runs
          the wrong directory forever, and never creates the nested archive.
    - [x] Implement GNU's two-phase remake behavior for existing stale included
          makefiles, not only missing ones. Dependency-graph construction is
          now non-destructive: Kati first updates parsed include targets,
          reparses only if a recipe actually ran, and otherwise proceeds to the
          requested goals. A focused regression reproduces Kbuild's stale
          `include/config/auto.conf` after `.config` changes; all 33 embedded
          Make/Brush cases pass.
    - [x] Give the parallel dependency scheduler an explicit active state.
          Once a target has been selected for preparation or execution, another
          release path cannot enqueue it again merely because its unfinished
          count is already zero. A shared/duplicate-prerequisite `-j10`
          regression verifies each recipe runs exactly once; all 34 embedded
          Make/Brush cases pass on native aarch64.
    - [x] Bound recipe preparation to the currently available execution slots.
          Recipe expansion now occurs when work can actually be dispatched,
          preserving GNU's `-j1` visibility of files created by an earlier
          recipe and preventing a large Kbuild graph from expanding thousands
          of archive recipes while all workers are idle. The focused ordering
          regression brings the native aarch64 Make/Brush suite to 35 cases.
    - [x] Make non-`-k` parallel failure draining terminal. Successful recipes
          already in flight may release new dependents after the first failure;
          those deliberately unstarted nodes no longer make the failed scheduler
          spin forever. The parallel-failure regression now includes exactly
          this late-release shape.
    - [x] Order viable implicit pattern rules by GNU specificity (shortest stem,
          with later-definition precedence only among ties). This lets ARM64
          Kbuild's `%.pi.o: %.o` chain beat the generic `%.o: %.S` chain instead
          of inventing a missing `idreg-override.pi.S`. A focused chained-rule
          fixture brings the native aarch64 Make/Brush suite to 36 cases.
    - [x] Compose target-specific variables from every matching pattern in
          broad-to-specific order, honoring `+=` across the independent scopes.
          ARM64 Kbuild's `lib-%.pi.o: OBJCOPYFLAGS += ...` now retains the common
          `%.pi.o` symbol-prefix flags instead of producing duplicate symbols at
          final link. The focused fixture brings the suite to 37 cases.
    - [x] Skip implicit-rule search for `.PHONY` targets, as GNU does, including
          targets declared only as `.PHONY` with no separate empty rule. This
          keeps a match-anything `%::` fallback from becoming the recipe of
          OpenWrt's `FORCE` target and recursively re-entering `make prereq`.
          The exact rule-less `%::`/`FORCE` fixture brings the suite to 38 cases.
    - [x] Preserve ordinary external-command identity for optimized in-process
          commands: `command -v` and `type` now discover their executable PATH,
          while execution remains optimized. Uutils clap help/version output is
          routed through the logical pipeline streams, the self-shadowed Bash
          version probe is truthful and successful, and `find --version`
          identifies its GNU-compatible contract. These were all exercised by
          OpenWrt's prerequisite probes in a real FUSE/Brush box.
    - [x] Make umask logical shell state instead of shared process state. A
          subshell's `umask 077` is cloned and isolated, external children get
          it in their pre-exec hook, and in-process utilities receive it through
          thread-local uucore context. A 100-way nested-umask/mkdir fixture keeps
          its parent at 0022 while producing 0700 directories and 0600 external
          files; OpenWrt's parallel proper-umask prerequisite now passes.
    - [ ] Complete the OpenWrt 25.12.5 `armsr/armv8` clean `make -j10 world`
          FUSE/Brush gate. The source is pinned at tag/commit
          `v25.12.5`/`f0a60eee2fe051741c643ea6118718aae1ef17fb`, with all 87
          required archives cached. Prerequisite checking has progressed into
          package metadata scanning. Kati now handles the unambiguous GNU
          long-option abbreviation generically and recognizes an `env bash`
          `SHELL` as the POSIX shell it actually invokes, so the make export
          prefix (including `TOPDIR`) reaches recursive package scans. The
          exact two-level `env bash` / compound subshell / literal `make -C`
          regression brings the Make/Brush suite to 39 cases. Kati statement
          evaluation now snapshots immutable parsed statement Arcs before
          releasing the parser-list mutex: OpenWrt's variable-guarded recursive
          includes can revisit a cached makefile and reach their inner guard
          instead of deadlocking on the outer include evaluation. The focused
          recursive-include regression brings the suite to 40 cases. A clean
          real `make -j10 prereq` now completes in 43 seconds, records 397
          processes, 5,627 Brush provenance rows and 3,734 build edges, and
          captures the 1,821,710-byte `.packageinfo` plus 1,103,452-byte
          `.targetinfo` without changing the source checkout. The first clean
          `world` attempt then reached zstd's upstream compile. Recursive make
          now consumes explicit inherited `--jobserver-auth` and
          `--jobserver-fds` control arguments without rejecting or propagating
          them (case 41). Kati's bootstrap now provides GNU's core C, C++, and
          preprocessed-assembler `COMPILE.*`, `LINK.*`, `PREPROCESS.S`, and
          `OUTPUT_OPTION` relations, and its suffix rules use those same
          definitions. Case 42 performs the exact dependency-producing
          `$(COMPILE.c) $(DEPFLAGS) $(OUTPUT_OPTION) $<` recipe that zstd uses;
          the complete native-aarch64 Make/Brush suite passes. A fresh `world`
          run then advanced for 7m40s through the parallel host-tool wave before
          xz exposed a Brush legacy-backquote bug: `\\` inside backquotes was
          incorrectly retained as two backslashes, so libtool's standard
          config.status double-eval lost the escaped quotes in its generated
          script. The parser now performs the POSIX two-to-one reduction. Its
          focused parser test and the real libtool fragment in Make/Brush case
          43 both pass; the generated assignment retains escapes and reparses.
          A second clean run passed xz and progressed for 8m31s through 20,550
          recorded build edges before ELFkickers exposed missing GNU built-in
          executable-link relations: its makefiles intentionally declare an
          object prerequisite but rely on `.o:` to supply the recipe. Kati's
          bootstrap now defines the bounded `.o`, `.c`, `.cc`, `.cpp`, and `.C`
          single-suffix link rules instead of an unbounded match-anything
          workaround. Its selector treats an explicitly declared prerequisite
          as immediately applicable even before that file has been built, so
          `tool: tool.o` selects object linking while a plain `tool.c` still
          selects direct source linking. Case 44 verifies both executable paths,
          including the actual selected object-link command. The complete 44
          case suite passes on native aarch64. The next boundary is resuming a
          clean `world` build past ELFkickers/sstrip. That clean `-j10` run did
          pass ELFkickers, xz/liblzma, mtools, and squashfs4 and entered
          LibreSSL, reaching 17,719 processes, 280,436 Brush provenance rows,
          24,910 build edges, 58,965 captured paths, a 337 MiB SQLar, and 527
          MiB of live upper files before OrbStack's kernel OOM killer killed the
          server. A controlled clean `-j4` replay with five-second telemetry
          showed anonymous server RSS growing from 40 MiB to 1.10 GiB in four
          minutes while its thread count stayed at 17--19: this was not a
          connection-thread leak. The canonical inode table did retain path
          identities after the kernel released its last lookup reference, so
          `FORGET` now reclaims zero-reference identities while preserving the
          root and independent open-handle lifetime. A clean replay showed that
          this was a real but secondary retention source: static musl's allocator
          still kept hundreds of freed size-class mappings resident. Sarun now
          uses statically linked mimalloc, whose empty-page purging fits this
          long-running, multithreaded allocation pattern. At the comparable
          four-minute point server RSS is 160 MiB instead of 1.10 GiB, with no
          swap and the same 19 threads. The static aarch64 build, all 44
          Make/Brush cases, and the full FUSE/QEMU backend equivalence/lifecycle
          suite pass with the reclamation change; the mimalloc build also passes
          all 44 Make/Brush cases. A subsequent clean `-j10 world` replay no
          longer OOMed: it ran for about twenty minutes, passed ELFkickers,
          xz/liblzma, lz4, mtools, squashfs4 and LibreSSL, and reached 20,105
          processes, 46,680 build edges, a 427 MiB SQLar and 605 MiB live upper.
          Peak server RSS was 2.56 GiB with no swap, so mimalloc is a large
          improvement but not proof of a flat long-run memory curve; remaining
          workload-proportional retention still needs measurement after the
          correctness gate completes. That replay stopped at Autoconf because
          OpenWrt intentionally runs `$(MAKE) --touch install-man1` before its
          ordinary compile, while embedded Kati did not implement GNU `-t` /
          `--touch`; the ignored preparatory failure left patched sources newer
          than the shipped manuals and caused an unavailable `help2man` recipe
          to run. Touch mode is now real engine behavior: it updates stale
          non-phony targets without expanding or executing their recipes and
          skips phony recipes, including in recursive makes and inherited
          `MAKEFLAGS`. The exact OpenWrt two-pass shape is Make/Brush case 45;
          the static aarch64 build and complete 45-case suite pass. The next
          clean `-j10 world` replay passed that Autoconf boundary and ran for
          1,201 seconds, reaching 20,173 processes, 47,138 build edges, 374,270
          Brush provenance rows and 69,984 captured paths. Peak server RSS was
          2,511,472 KiB with only 1,348 KiB swapped. It then exposed Automake's
          external-parent shell shape: autom4te uses Perl backticks to capture
          `sh -c 'm4 2>&1 >/dev/null'`. Brush represented `2>&1` correctly in
          its logical fd table, but conversion of an `OpenFile::Stdout` stored
          in child fd 2 used `Stdio::inherit()`, which re-inherited parent fd 2
          instead of duplicating the actual fd 1 pipe. The m4 definitions leaked
          to the terminal and Perl received an empty string, making Automake
          falsely report a missing `AM_INIT_AUTOMAKE`. Standard-stream values
          now materialize their actual descriptor when installed in a different
          child slot. The exact external-Perl/shadow-sh regression passes in the
          released-binary nested-shell suite, all 45 Make/Brush cases still
          pass, the static aarch64 build passes, and the captured OpenWrt box
          now configures and builds `tools/automake`. A subsequent genuinely
          clean `-j10 world` replay ran for 21m55s and reached 20,789 processes,
          48,012 build edges, 382,806 Brush provenance rows, and 101,918 paths.
          Peak server RSS was 2,495,700 KiB; the end state retained 2,382,128
          KiB RSS and used 240,164 KiB swap, so workload-proportional retention
          remains an explicit investigation after the correctness gate. That
          replay passed both Autoconf and Automake, then exposed two independent
          host-tool failures. CMake's bootstrap uses legacy backquotes around
          `cmake_escape_artifact \"${h}\"`; Brush kept the escaped quotes as
          literal argument bytes, producing quoted dependency path names in
          `build.ninja`. Legacy-backquote parsing now removes the escape and
          lets those quotes group the nested command's argument. The focused
          parser test and CMake-shaped end-to-end Make/Brush case 46 pass on the
          static aarch64 engine. Libtool's independent recursive-bootstrap
          failure came from treating the command-line goal
          `./libltdl/Makefile.am` as distinct from its normalized
          `libltdl/Makefile.am:` rule. Kati now gives command-line goals the same
          leading-`./` canonical identity as parsed rule targets; focused case
          47 and Libtool's real `bootstrap-deps` invocation pass. That exposed a
          second generic boundary in `ltmain.sh` generation: an interposed
          shebang script used as a non-final pipeline stage was awaited before
          Brush spawned its reader, so output larger than a pipe buffer
          deadlocked. Eligible interposed pipeline stages now return a waitable
          task and execute concurrently. The released-binary nested-shell test
          sends 220,000 bytes through such a producer, and the real Libtool
          bootstrap now generates `libltdl/Makefile.am`, `m4/ltversion.m4`, and
          `build-aux/ltmain.sh`. CMake then exposed the other half of legacy
          backquote escape processing. Its source-specific bootstrap flags use
          ``eval echo \\${cmake_c_flags_\${a}}``: the doubled escape must keep
          the outer parameter literal for `eval`, while the single escape must
          let `${a}` expand in the command substitution. Brush preserved both,
          leaving literal `${cmake_c_flags_String}` in `build.ninja`; `String.c`
          consequently compiled as an empty object without
          `-DKWSYS_STRING_C`. Legacy backquotes now remove the single escaped
          dollar as Bash/dash do. The exact parser unit and released-binary
          Make/Brush case 48 pass. From an empty `Bootstrap.cmk`, the real
          OpenWrt CMake bootstrap now generates correct flags, compiles and
          links all 309 objects, completes its 109-second second-stage configure,
          and exits zero. The static aarch64 build, all 48 Make/Brush cases, and
          the complete nested-shell suite pass. A genuinely clean `-j10 world`
          replay then ran for 1,691 seconds and reached 29,202 processes,
          48,421 build edges, 391,767 Brush provenance rows, and 116,985
          captured paths. Peak server RSS was 2,597,632 KiB with 1,734,276 KiB
          swapped; the host was also running unrelated memory-heavy work, so
          this is a pressure observation rather than an isolated benchmark.
          CMake's full 309-object bootstrap, second-stage configure, optimized
          build, and install all completed. The parallel Libtool branch exposed
          a generic makefile-remake convergence bug: Kati restarted whenever
          any command in an included makefile's dependency graph ran. Libtool's
          revision recipe intentionally runs and leaves `m4/ltversion.m4` (and
          therefore the included `Makefile`) unchanged when already current;
          GNU make observes the unchanged makefile timestamp and proceeds, but
          Kati exhausted its five-pass guard. Remake convergence now snapshots
          each included target before execution and restarts only when its own
          mtime/size changes. Make/Brush case 49 covers an always-run recipe
          which intentionally leaves its included target untouched; the full
          49-case suite passes. Replaying `tools/libtool/compile` in the same
          captured box now builds, links, installs, stamps, and exits zero. The
          next boundary was resuming `world` from that state. That continuation
          ran for about 42 minutes before `tools/mklibs` reached a missing
          `configure`: its best-effort autoreconf had invoked the OpenWrt
          `aclocal` wrapper without `STAGING_DIR_HOST`, resolving
          `/bin/aclocal.real`. Patchelf, fakeroot, and flex showed the same
          missing export concurrently, but retained shipped configure scripts.
          An eight-way recursive-make stress fixture, including OpenWrt's exact
          `scripts/time.pl` wrapper, preserves the export in every child and
          completes every autoreconf. More importantly, cleaning those four
          real tools and replaying the actual `make -j10 V=s tools/install`
          graph in the captured box passes all four concurrent autoreconf jobs,
          builds and installs mklibs, then continues through MPFR, MPC, Bison,
          erofs-utils, e2fsprogs, findutils, and elfutils. Elfutils exposed two
          generic dependency-composition gaps: prerequisite-only explicit rules
          such as `i386_lex.o: i386_parse.h` must compose with `.l.c` and `.c.o`,
          and a pattern prerequisite may itself be produced by a suffix rule.
          Kati now selects those chains after direct pattern candidates; the
          real replay generates the parser and scanner, builds both `.o` and
          `.os` variants, and passes their former link boundaries. Make/Brush
          case 50 pins all three relation shapes. That replay then showed that
          recursive `MAKEFLAGS` serialization collapsed OpenWrt's ordered set
          of command-line `LIBS+=...` relations to its final member, omitting
          `libgnu.a` from elfutils links. Accumulative command-line definitions
          are now retained in order (without duplicating inherited entries at
          every recursion), while ordinary assignments still replace earlier
          values. Case 51 exercises late child-scope expansion of the preserved
          appends. The focused real replay now links all eight elfutils host
          programs with `libgnu.a`, installs them, stamps the tool, and exits
          zero. Resuming `tools/install` then exposed two adjacent escaping
          contexts. Quilt's configure uses an unquoted legacy backquote whose
          escaped double quotes must remain command text, while the earlier
          CMake form places the backquote inside double quotes and consumes the
          same escape. Brush's word relation now distinguishes those contexts.
          U-Boot's `filechk_config_h` uses `\#include` inside `$(if ...)`; GNU
          preserves function-argument escapes until the generated recipe is
          parsed, but Kati had removed the slash and turned the rest of the
          one-line recipe (including its closing parenthesis) into a comment.
          Function arguments now preserve `\#` and `\\`, with escape/comment
          handling owned by the surrounding expression. The exact Quilt and
          U-Boot relations are cases 52 and 53. The real U-Boot target now
          emits `include/config.h`, preprocesses `u-boot.cfg`, regenerates
          `include/autoconf.mk`, and exits zero. The static aarch64 build,
          parser units, vendor reproduction checks, and complete 53-case suite
          pass. The full `make -j10 V=s tools/install` replay now configures,
          builds, installs, and stamps both Quilt and U-Boot (including
          `mkimage` and `mkenvimage`) and exits zero. The host-tool checkpoint
          is complete; the next checkpoint is resumption of `world`. That
          resumed graph reached the parallel GDB 16.3 and Binutils 2.44 target
          toolchain builds, then both BFD copies tried to remake the shipped
          `doc/bfdsumm.texi` through `doc/%.texi: doc/%.stamp`. Kati's shallow
          chain test saw matching `doc/%.stamp: %.c` and `: %.h` rules and
          selected the outer relation without proving that either terminal
          `bfdsumm.c` or `bfdsumm.h` existed. Implicit-rule viability now walks
          the complete candidate chain, with per-branch cycle/rule guards,
          before admitting it to the graph. Valid same-stem BFD documentation
          chains remain active. Make/Brush case 54 and a native Kati/GNU corpus
          fixture pin the shipped-generated-file behavior; vendor reproduction,
          the static aarch64 build, and the full 54-case suite pass. A focused
          replay of both released toolchain targets passed their former
          `bfdsumm.stamp` failure; GDB then built and installed completely.
          Binutils advanced through BFD and exposed the next generic GNU-make
          relation at GProf: Automake's recipe-less `%.o: %.m` cancellation was
          treated as a viable empty recipe, so three missing `*_bl.o` files
          reached the linker instead of using the valid `.c.o` suffix rule.
          Recipe-less patterns now remove an otherwise identical implicit rule
          and never enter candidate selection themselves. A native Kati/GNU
          corpus fixture covers both the Automake cancellation shape and
          cancellation of an earlier user pattern; Make/Brush case 55 covers
          the same relations at `-j10`. Vendor reproduction, the static
          aarch64 build, and the complete 55-case suite pass. The focused
          Binutils replay now passes end-to-end: it builds and links GProf,
          generates both AArch64 and 32-bit ARM linker emulations, installs the
          complete Binutils 2.44 toolchain, and exits zero under Brush at
          `-j10`. The resumed `world` graph reached initial GCC 14.3.0 and
          exposed a generic pathname-expansion error: for
          `gcc/*/config-lang.in`, Brush admitted the regular file
          `gcc/ABOUT-GCC-NLS` as a wildcard prefix and appended the literal
          suffix without proving the complete pathname existed. Glob expansion
          now validates the final path with no-follow metadata, retaining valid
          dangling-symlink matches while rejecting impossible literal suffixes.
          A direct Brush-core unit and the real-box Bash comparison corpus pin
          the GCC shape; vendor reproduction, the static aarch64 build, and all
          47 Brush conformance probes pass. The first focused GCC replay appeared
          to exit zero, but artifact inspection correctly rejected that result:
          GCC recipes use `exec`, and Sarun had created each embedded recipe as
          a root Brush shell. Brush therefore replaced the entire engine process
          at a nested `exec`, silently abandoning the outer build and install
          recipes. Embedded make/ninja recipe shells are now explicitly logical
          subprocesses, so `exec` terminates only that recipe shell. Make/Brush
          case 56 exercises the external-wrapper, recursive-make, nested-`exec`,
          build-stamp, and install-stamp boundary; all 56 cases pass and the
          Brush vendor series reproduces exactly. The real OpenWrt focused
          replay then ran for 240 seconds at `-j10`, built and installed GCC
          14.3's initial AArch64 cross compiler and target libgcc, created both
          `.built` and `.gcc_initial_installed`, and a separate box invocation
          executed the installed 2,156,368-byte
          `aarch64-openwrt-linux-musl-gcc`. Resuming `world` then exposed that
          embedded recipes inherited the host terminal as fd 0 instead of the
          make invocation's logical stdin: OpenWrt's `yes '' | make oldconfig`
          blocked forever with `yes` filling its unread pipe. Logical make
          stdin now follows the Evaluator/Executor boundary into parallel Kati
          workers and is installed as each Brush recipe's fd 0. Make/Brush case
          57 covers an exact piped recursive-make/read shape; all 57 cases and
          both vendor reproduction checks pass. The real OpenWrt questionnaire
          now consumes the pipe and completes. Its next kernel-header pass found
          a filesystem type-replacement bug rather than a make rule failure:
          tar unlinked then recreated
          `arch/arm64/tools/syscall_64.tbl`, but `set_symlink` updated only the
          old whiteout row's payload and size, leaving its mode as a tombstone.
          Symlink replacement now clears stale node metadata and resets all
          type-bearing SQLar columns, provenance, and opacity; replacing a
          regular file also removes its stale blob. A focused depot regression
          covers both whiteout and regular-file replacement. In a clean real
          extraction the relative syscall-table symlink is visible, `oldconfig`
          completes, `SYSHDR` generates `unistd_64.h`, and `headers_install`
          consumes the generated AArch64 ABI headers. The clean focused target
          exits zero after 1,079.66 seconds, creates both `.built` and
          `.linux_installed`, and an independent box verifies the live symlink,
          generated header, and stamps. The complete 57-case Make/Brush suite
          also passes against this engine. Resuming `world` reached final GCC
          14.3.0, where a third-level recursive make incorrectly retained the
          top-level command-line `CXX` after its middle makefile intentionally
          set `MAKEOVERRIDES =`. Embedded Kati now seeds and reconciles
          `MAKEOVERRIDES` at each parse boundary, so command-line variables cross
          only the recursive boundaries selected by the makefiles. Make/Brush
          case 58 pins the exact parent/middle/leaf origin transition. Removing
          the failed real `array_type_info` object and replaying the focused GCC
          target then emitted the complete `xgcc` command and rebuilt the object
          successfully. The full OpenWrt final-GCC wrapper proceeded through
          compilation and install, then exposed one more generic GNU built-in:
          OpenWrt uses `$(RM)` without defining it, so an empty expansion tried
          to execute `lib/libiberty.a`. Both embedded and standalone Kati now
          provide GNU's default `RM=rm -f` unless built-in variables are disabled;
          Make/Brush case 59 verifies the actual removal. Vendor reproduction,
          the static aarch64 build, and all 59 Make/Brush cases pass. Replaying
          the preserved final-GCC wrapper crossed the former boundary, patched
          the toolchain specs, and created both `.built` and
          `.gcc_final_installed`. An independent Brush box executed the installed
          OpenWrt GCC 14.3.0 C++ driver and compiled a C++20 translation unit into
          a 64-bit AArch64 ELF relocatable object. Resuming `world` for 1,571
          seconds extracted and patched Linux 6.12.94, installed its UAPI
          headers, generated both AArch64 and compat32 syscall tables, and
          reached the target kernel compile. ARM64's stack-protector setup uses
          a tab between the make function name and argument in a multiline
          `$(shell ...)`; Kati scanned that tab into the variable name and left
          the outer `)` as a recipe. Function-name scanning now treats every
          ASCII whitespace byte as GNU make does. The exact multiline Linux
          shape is Make/Brush case 60; the Kati unit, vendor reproduction,
          static aarch64 build, and all 60 Make/Brush cases pass. The preserved
          real Linux tree now completes `ARCH=arm64 stack_protector_prepare`
          with the eval consumed as make syntax. The next real gate is the
          parallel `target/linux/compile` continuation, followed by `world`.
          That continuation compiled the complete built-in and module object
          graph, created `vmlinux.a`, and reached `MODPOST` after about 28
          minutes. Modpost then found `__stack_chk_guard` references in the
          modules: recipe-time `$(eval)` had updated Kati's variable store, but
          recursive makes still inherited the exported-variable prefix
          materialized after parsing. Kati now refreshes that evaluator-owned
          prefix immediately after an eval, preserving the exact GNU sequence
          point without process-global environment writes. Make/Brush case 61
          exports a variable, changes it from a prerequisite recipe, and checks
          its value and environment origin in a later recursive make. The
          static aarch64 build and all 61 cases pass. The next gate is replaying
          the target kernel so Kbuild's command-change tracking recompiles the
          affected objects with per-task stack-canary flags, then crossing
          modpost and resuming `world`. The first replay exposed the adjacent
          scheduler relation: the commandless phony `prepare` target completed
          as a missing filesystem timestamp, so its existing-directory consumer
          `.` was considered current and the recursive object build did not run.
          A phony completion now has explicit freshness newer than every real
          timestamp, including when its recipe expands to nothing; consumers
          are consequently rebuilt as GNU requires. Case 62 pins Kbuild's exact
          `all modules` / shared prepare / literal `.` / recursive-build shape.
          The complete 47-test Kati unit suite (including its corrected
          single-suffix candidate expectation), static aarch64 build, vendor
          reproduction check, and all 62 Make/Brush cases pass. The next gate
          remains the real kernel replay and modpost. That replay did cross the
          phony boundary and re-enter Kbuild with all three per-task canary
          options, but exposed a second half of the assignment relation: the
          appended options replaced the previously visible global
          `KBUILD_CFLAGS`, dropping `-O2`, `-std=gnu11`, and
          `-Wno-address-of-packed-member`. Kati's `+=` now reads from the full
          variable view while writing to the assignment's declared scope. This
          matches GNU make for both recipe-time global eval and target-specific
          append, and detaches the value when the visible and destination
          bindings differ. Cases 61 and 62 now initialize realistic baseline
          flags and require the recursive child to receive the baseline plus
          the canary append. The 47 Kati units, vendor reproduction, static
          aarch64 build, and all 62 Make/Brush cases pass with that stronger
          assertion. The preserved real kernel replay then recorded the full
          baseline flags and all three canary flags in every invalidated Kbuild
          command, rebuilt 9,129 affected compiler steps at `-j10`, crossed
          `MODPOST` without an undefined `__stack_chk_guard`, linked the module
          set and final `vmlinux`, and exited zero after 2,089 seconds. The
          captured outputs include a 107,768,960-byte `vmlinux`, 20,472,320-byte
          AArch64 `Image`, 8,377,549-byte `Image.gz`, 3,585,082-byte
          `System.map`, and the `.modules` stamp, with concrete compiler/linker
          writer records. At this checkpoint the box archive holds 148,537
          processes, 2,037,330 Brush provenance rows, and 931,357 build edges.
          A complete `world` continuation then re-entered the already-built
          kernel. Its incremental `target/linux/compile` completed and relinked
          `vmlinux`, `Image`, and `Image.gz`, but took 2,373 seconds wall for
          only 44 seconds user and 56 seconds system time in the wrapper. The
          recursive Kati process accumulated about 81 seconds of CPU while the
          server stayed near one full core: this is a filesystem-service
          bottleneck, not a hidden rebuild or dependency-evaluator stall. Two
          independent amplifiers were found. First, Kati emitted one "Nothing
          to be done" line per current root, flooding Brush capture. Case 63
          pins explicit `-s`, but Kbuild selects the same makefile-wide mode
          with a prerequisite-free `.SILENT:` special target. Global `.SILENT:`
          now suppresses both recipe echo and no-op root diagnostics;
          target-specific `.SILENT: target ...` is carried as an ordinary set
          of target identities through recipe evaluation and no-op reporting.
          End-to-end Make/Brush cases 64 and 65 pin the global and selective
          forms. Second, each ordinary lower lookup called
          `BackingStore::exists()` and then `BackingStore::attr()`, causing two
          complete root-to-leaf PassthroughFsRo walks. Attribute resolution now
          performs one metadata probe and reuses it for the merge decision and
          returned attributes. A timing replay then exposed another
          per-attribute tax: `atime_of` and `owner_of` each queried the growing
          sqlar database even when no override existed. Atime and ownership now
          have coherent in-RAM mirrors,
          populated once on box hydration and maintained by create, reload,
          rename, subtree reparent, replacement, and deletion paths; SQLite
          remains their durable representation rather than their lookup path.
          A focused mutation/reopen/reload regression pins mirror coherence.
          The decisive remaining cost was recipe-state attribution rather than
          inode lookup: the archive contained about 1.4 million `build_edges`,
          while every start/done event searched the JSON primary output or
          command without an index. At 22 minutes the server had performed 272
          million read syscalls and 1.1 TB of logical reads against the 3.4 GB
          archive while Kati itself had used only 37 CPU seconds. Expression
          indexes now cover primary-output and command transitions. Transition
          updates also select the newest matching edge, fixing attribution when
          a rerun appends a graph containing the same output or command as a
          historical graph. A focused regression pins both the indexes and
          newest-graph semantics. On the resumed real workload recursive Kati
          rose from roughly 3% to 85% CPU and crossed the kernel phase in the
          first few minutes instead of spending tens of minutes in database
          scans; the server can now serve concurrent compilation at more than
          one core. Vendor reconstruction, all 47 Kati units, the static
          aarch64 build, and all 66 Make/Brush cases pass. After the kernel the
          same run built and installed fwtool, usign, libjson-c, and GRUB; it
          exited 2 only because the deliberately offline fixture lacked the
          checksum-pinned Lua 5.1.5 and ncurses 6.4 archives. Both archives are
          now present in the persistent lower `dl/` cache with OpenWrt's exact
          expected SHA-256 values. The resumed `world` run, with host networking
          available for any further cache misses, crossed
          `target/linux/compile` and built host ncurses, Lua, libubox, GRUB,
          fwtool, usign, and libjson-c in parallel. Lua install then exposed a
          recursive-make environment bug: OpenWrt's `override MAKEFLAGS=` had
          removed the command-variable slot, but reconciliation unconditionally
          reinserted `MAKEOVERRIDES`, leaking top-level `V=s` into Lua's own
          `V=5.1` filename suffix and producing nonexistent `luas.1`. Overrides
          are now spliced back only while the evaluated MAKEFLAGS still contains
          its `--` slot. A first two-level regression was too shallow: the real
          package evaluator did clear MAKEFLAGS, but its fresh embedded Brush
          shell retained the top-level ambient value because an empty export was
          omitted. Recipe prefixes now explicitly export an empty MAKEFLAGS (or
          unset it when absent), so clearing the recursive boundary cannot leave
          stale command variables in the shell. Case 66 now pins the real
          three-level shape: top-level `V=s`, a middle makefile with `override
          MAKEFLAGS=`, and Lua-like child `V=5.1`. Vendor reconstruction, all 47
          Kati units, the static aarch64 build, and all 66 Make/Brush cases pass.
          A clean rebuild of the actual OpenWrt Lua package through Brush also
          compiles and installs `lua5.1`, `luac5.1`, headers, manuals, pkg-config
          metadata, symlinks, and the host-package stamp. The resumed
          `world -j10` crossed Lua and the repaired fakeroot stage, packaged the
          target C runtime, and held the repaired kernel replay near 130 seconds
          (versus 2,373 seconds before the build-edge indexes). Kernel-module
          packaging then exposed a separate interposed-exec semantic leak:
          OpenWrt's `rstrip.sh` sets `IFS=:` inside its read-loop subshell and
          invokes `strip-kmod.sh` as a fresh interpreter. The in-process shebang
          optimization had cloned the caller's whole shell state, so the child
          passed its unquoted `$ARGS` to `objcopy` as one space-containing
          argument; its following unconditional `exit 0` hid the failed strip.
          A snooped interpreter now recreates a real exec boundary: only set,
          exported variables cross as strings; well-known variables including
          default IFS are initialized afresh; POSIX shells import no functions
          while bash imports only explicitly exported functions; and aliases,
          directory stack, path cache, and status bookkeeping are cleared. The
          exact regression observes three split arguments, hidden unexported X,
          and preserved exported Y. All thirteen nested-shell integration cases,
          vendor reconstruction, the static aarch64 build, and all 66
          Make/Brush cases pass. The tainted module packages from the
          interrupted run were then removed and rebuilt through Brush at
          `-j10`. All 105 packaged modules and 96 kernel APKs were produced,
          no `.tmp` module remained, and a sampled AArch64 module had neither a
          `.comment` nor a `.note.gnu.build-id` section; no malformed `objcopy`
          invocation recurred. The focused package rebuild took 779 seconds
          wall while the runner itself used only 0.06 CPU seconds and the
          server stayed near one core. Unlike the eliminated build-edge scans,
          this remaining cost is dominated by the serialized stream of small
          package-copy, metadata, overlay, and provenance operations through
          the FUSE service. A resumed `world -j10` ran for 5,023 seconds before
          the server exited and left its process tree waiting on the aborted
          mount. SQLite recovered the interrupted 6.8 MB rollback journal and
          `PRAGMA quick_check` found the 4.6 GB archive intact. A bounded live
          server backtrace then separated the remaining costs precisely:
          multiple FUSE workers were simultaneously in
          `BoxState::children_of`, scanning every captured path with string
          prefix tests for each directory open. The layer mirror now maintains
          a direct-child index alongside exact-path lookup; creation, node-kind
          replacement, removal, rename, subtree reparent, reload, and full
          hydration all keep it coherent. Directory enumeration is therefore
          proportional to immediate children rather than the complete layer.
          All nine depot tests, both merged-layer tests, the focused child
          transition/reload regression, and the static aarch64 build pass. In
          the indexed real-workload replay every FUSE worker was idle in
          `poll`; the newly exposed bottleneck was a single SQLite autocommit
          deleting the rollback journal while seven provenance/build-edge
          handlers waited on the per-box connection mutex. After 631.8 s that
          replay had accumulated 3,592,064 build-edge rows and 2,331,703
          pipeline rows, occupied one full engine core, and left actual package
          recipes stalled for more than five minutes. The recorder, not FUSE
          transport, was the limiting work. Box databases now use one bounded
          PERSIST rollback journal instead of creating and unlinking a DELETE
          journal for every event. This retains interrupted-process rollback
          recovery, avoids WAL checkpoint state, and removes the exact commit
          syscall seen in the stack. Its policy regression, all nine depot
          tests, both merged-layer tests, and the static aarch64 build pass.
          A 296.0 s PERSIST replay confirmed the journal unlink was gone, but
          the hot handler still committed individual database pages while its
          peer waited on the connection mutex; it added more than 300,000 graph
          rows in roughly two minutes. The wire boundary already sends each
          complete Kati/ninja graph, make-variable set, and pipeline-completion
          set as a batch. Capture now preserves those batches as atomic SQLite
          transactions instead of exploding them into per-row autocommits.
          The production static aarch64 build, its static test harness, all nine
          depot tests, and both merged-layer tests pass. The next replay
          showed that remaining single-event pipeline traffic still copied
          database pages into the PERSIST rollback journal while FUSE and
          recorder handlers waited on the same connection. The live recorder
          now has one WAL policy: writes append without rollback-page copying,
          automatic checkpoints operate in 64 MiB batches, and SQLite recovers
          an interrupted engine from the WAL. There is no runtime mode or
          compatibility branch. In the aarch64 OpenWrt `world -j10` replay the
          WAL stayed at its checkpoint boundary, rollback-journal stacks
          disappeared, and host-package recipes began running concurrently.
          That recipe-phase sample exposed a separate historical-table scan:
          every stderr attribution fixup searched all `outputs` rows. The
          schema now indexes its `(stream, ts)` predicate, keeping attribution
          work proportional to the current recipe's time window. A subsequent
          package-extraction sample found ordinary FUSE `flush` implemented as
          `fsync`, forcing durability on every close. `flush` now mirrors
          close with `dup`/`close`, as the upstream virtiofsd passthrough does;
          explicit `fsync` remains the durability boundary. The resumed replay
          contained no `fsync`/`sync_all` close stacks and advanced from host
          tools into target kernel-module packaging in under five minutes.
          At target-package concurrency, five provenance completions and FUSE
          metadata readers could still wait behind one recorder append because
          both domains shared a Rust mutex around one SQLite connection. WAL
          could not provide read/write concurrency through that mutex. Capture
          now retains one ordered recorder writer on a distinct WAL connection,
          while filesystem metadata uses the original connection and reads its
          last committed snapshot concurrently. A regression holds an
          uncommitted recorder transaction, proves the overlay connection mutex
          remains available, and proves the reader cannot see the uncommitted
          row. The production binary, static aarch64 test harness, all eleven
          depot tests, both merged-layer tests, the canonical virtio lifecycle
          test, and the binary action socket test pass.
          Earlier nonfatal empty-operand arithmetic and generated-config `sed`
          diagnostics stay recorded for attribution rather than normalization.
    - [x] Complete the native-aarch64 FUSE Brush gate from a clean output tree.
          Linux 6.18 builds 823 objects with `-j10` (11 observed overlapping
          clang processes), takes 162 s wall / 143 s compile, and records 2,797
          process rows, 43,513 `brushprov` rows, 4,276 build edges, and 63,019
          output bytes. The captured 5,320,862-byte Image and 6,089,312-byte
          vmlinux have writer provenance and SHA-256
          `8272075e0b226fa543c4031bf83158a175e9dc56e18f47db3b5f773d7055a8ef`
          and `3b65a88b2f5e8f25840eeaeb9009c3adac8e163fe7f2ade82eb426e6eef0d73e`;
          lower/output/source escape checks all pass.
    - [ ] Run the complete QEMU Brush gate under KVM and compare its artifacts
          with the FUSE Brush result. This aarch64 OrbStack host exposes no KVM;
          the hour-long TCG result is not a useful substitute for that remaining
          performance/conformance gate.
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
- `make test-backends` passes live FUSE/QEMU equivalence on aarch64. The current
  architecture-correct run of the broader historical Python suite collected
  55 tests and produced 12 pass, 1 skip, 42 fail; most failures are explicit
  legacy harness assumptions (`/root` fixtures as uid 501 and default Tap on a
  host whose kernel rejects unprivileged user namespaces), with additional
  stale API/baseline cases. Those are recorded work, not treated as backend
  failures or silently converted to green skips.
- The same gate executes real ARM EABI and i386 static ELF32 probes under the
  aarch64 and x86_64 paired kernels, respectively.  Both print the exact
  sentinel and return exit 32 on this aarch64 TCG host.  This catches missing
  compatibility loaders or syscall ABIs; inspecting kernel config alone is not
  accepted as proof.
- `make test-backends` also launches QEMU from inside a FUSE box through the
  authenticated broker and descriptor-only appliance boundary. It checks the
  persisted parent edge and child archive, not merely a successful boot.
- A QEMU guest can itself issue `run --qemu`; the request returns to its live
  host outer runner and launches a flat sibling QEMU process. The aarch64 TCG
  gate records the outer box as parent, relays stdin/EOF and ordered output,
  returns exact TERM status, captures the child's write only in its archive,
  waits for a background flat child at outer teardown, and leaves the lower
  host tree unchanged. It does not execute QEMU in the guest.
- The same gate runs the aarch64 QEMU lifecycle matrix, including an immediate
  same-name rerun that must observe prior captured state. This regression found
  and now prevents both stale running-box registration and retained frontend-fd
  teardown races.
- `make test-backend-workloads` passes every strict real-tool stage on both
  FUSE and aarch64 QEMU/TCG, compares equal backend observations, and proves
  the caller-writable lower trees remain byte-for-byte and metadata unchanged.
- Normal static-engine and target-init builds now place a deterministic Cargo
  package/notice inventory plus the pinned SWI-Prolog and zlib notices in an
  adjacent `LICENSES` directory. Appliance builds also install Linux COPYING,
  QEMU GPL/LGPL/license guidance, and libslirp copyright beside their cached
  kernel and host-QEMU artifacts. Vendored workspace license links are
  dereferenced during assembly, so the notice bundle cannot inherit the old
  broken `LICENSE -> ../LICENSE` links.
- Completed Linux appliance object trees for both aarch64 and x86_64 have been
  removed from the build cache after publication. The paired QEMU build trees
  and the 730 MiB versioned appliance output remain; the obsolete 1.3 GiB
  OpenWrt diagnostic fixture was also removed before the clean replay.

## Commit gates

Commit and push after: this roadmap; static upstream seam; shared-core protocol
tests; FUSE parity/cutover; SUD ring; SUD parity/deletion; reproducible appliance
builders; aarch64 appliance; x86_64 appliance; final equivalence and cleanup.
During any long gate, commit and push a compiling WIP checkpoint rather than
holding hours of work only in the worktree.

The one-command local/external-hardware gate is `make validate-backends`: it
builds both tightly paired appliances and the host engine, runs portable
backend equivalence (including the ELF32 process gates and native SUD when the
host supports it), runs the strict real-project workload matrix, then prints
three-round benchmark medians.  Re-run only the timing portion with a larger
sample as `SARUN_BENCH_ROUNDS=5 make bench-backends`.  On physical x86_64,
read/write access to `/dev/kvm` causes the native x86_64 appliance to select
KVM, and the test fails if the accelerator marker does not confirm that choice.
Use `make validate-backends-kvm` on that machine to require accessible KVM and
make absence or fallback a hard failure instead of accepting the TCG fallback.

## Known baseline failures

- The 2026-07-17 full static aarch64 unit run now passes 386 tests, ignores one,
  and exposed two pre-existing Brush/editor semantic-completion assertions:
  `production_brush_document_propagates_later_find_type_constraint` and
  `bash_editor_uses_relation_for_backward_completion_and_insertion`.  The
  QEMU appliance, generated-wire, and backend lifecycle subsets are green.
  These failures require finishing the declarative `find` argument grammar;
  they are not hidden by filesystem-backend test runs.
