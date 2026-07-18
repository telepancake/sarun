# Linux processes as GDB inferiors

## Outcome

Viros will present tasks from a locally built Linux guest as genuine GDB
remote-process inferiors.  GDB must be able to select a process, read its
memory and registers, relocate its executable, and use breakpoints owned by
that process without running `gdbserver` in userspace.

The implementation is split at a deliberately narrow boundary:

```text
GDB multiprocess RSP
        |
inferior facade -- LinuxOracle -- injected per-kernel probe
        |
QEMU system RSP (vCPUs and physical memory)
```

The facade and its GDB semantics contain no Linux structure offsets.  A probe
compiled by the exact kernel Kbuild tree supplies a frozen task snapshot using
a stable viros ABI.  Architecture-specific code is limited to reversible probe
execution and saved-register conversion.

## Kernel probe

The probe is an audited, freestanding relocatable object.  It has no unresolved
symbols, allocation, logging, locks, or calls into the kernel.  Its entry point
receives the live `init_task` address and caller-owned request/result buffers,
walks the frozen task list, and returns fixed-width records.

The first ABI supplies:

- stable task address, PID, TGID, parent and group leader;
- `mm` and page-table-root addresses;
- command name, CPU and scheduling state;
- ELF auxiliary-vector values needed for executable relocation; and
- target pointer width, byte order and userspace ABI width.

Capability-gated follow-on operations translate checked userspace mappings and
read a non-current native AArch64 task's saved EL0 x0-x30, SP, PC, and PSTATE
frame. The latter rejects current/on-CPU tasks, kernel threads, compat32 tasks,
stale task/mm/start-cookie identities, and malformed exception frames.

List links and record counts are validated.  A snapshot interrupted during a
list update is rejected and retried at another safe kernel boundary rather
than exposed as a partially plausible process list.

The probe is built as a companion artifact; it need not be installed in the
guest or permanently linked into the kernel.  The public artifact chain is an
exact-Kbuild record, an absolute-linked package, and a call-gate manifest.
Each handoff verifies the probe hash and carries the matching `vmlinux`
SHA-256 and GNU build ID; the final manifest generator rejects a different
kernel.  The package also records the target architecture, ABI version, load
address, entry/completion offsets, and payload hash.

## Reversible call gate

AArch64 is the first implementation.  The `inferiors openwrt-arm64` launcher
boots a user-supplied matching kernel stopped and uses the manifest-bound
`vmlinux` to reach the stable `ret_to_user` boundary before handing QEMU's sole
RSP connection to the facade.  The call gate saves that stopped context and
enters the manifest-declared EL1h context and scratch stack.

One vCPU transaction:

1. Stop all vCPUs and save their architectural state.
2. Save selected executable, writable-result, and stack pages.
3. Write the linked probe and empty result area through QEMU physical-memory
   mode.
4. Set the selected vCPU's PC, SP, argument registers and masked PSTATE.
5. Resume to a QEMU hardware breakpoint at the probe completion PC.
6. Validate and copy the result.
7. Restore every page, register, breakpoint and QEMU memory mode in `finally`.
8. Re-read and compare restored state before returning a snapshot.

The probe returns to a `brk` completion symbol, with a QEMU hardware breakpoint
at that address intended to stop it before the instruction executes.  The
facade's direct RSP backend applies the manifest timeout as a host wall-clock
bound, interrupts QEMU on expiry, consumes the stop, and then restores state in
`finally`.  This is not an emulated-instruction budget.  The separate
interactive GDB-Python command has no wall-clock bound and relies on `Ctrl-C`
before its restoring unwind.

Before mutation the runtime also checks that GDB has the manifest-bound
`vmlinux` loaded and asks QEMU to verify each declared virtual-to-physical
mapping.  It cannot yet authenticate the running guest kernel by reading its
build ID from guest memory; the operator must ensure that the VM booted the
same kernel image.

Ordinary QEMU resume advances virtual time.  A later small QEMU call-gate
packet can execute with an instruction budget while clocks/devices remain
stopped, explicitly invalidate translated blocks, and make the operation
time-neutral.  The kernel probe ABI does not depend on that optimization.

## Multiprocess RSP facade

Upstream thread IDs use `pTGID.TID`.  Stock GDB creates inferiors when those
identities appear in `qXfer:threads:read`; downstream QEMU continues to expose
only vCPUs.

The first all-stop facade implements the packet and selection layer for:

- `qSupported`, `qXfer:threads`, `qfThreadInfo`, `qC`, `qAttached`, and `T`;
- `Hg`/`Hc` logical process selection;
- `qXfer:auxv` and `qXfer:exec-file` from the frozen snapshot;
- QEMU's complete registers for a current task, saved native AArch64 core
  registers for a sleeping task, and checked read-only selected-process memory
  when the sealed probe advertises the corresponding capabilities;
- ref-counted global QEMU breakpoints with per-process ownership; and
- `vCont;c` plus current-task-only single stepping.

The facade never forwards a synthetic Linux PID to QEMU.  The completed stop
filtering layer refreshes the live probe oracle, attributes a vCPU to a Linux
task, reports an owned breakpoint, or consumes the stop and resumes.  This is
wired into the launcher, while an exact matching local-OpenWrt live proof is
still required.
Kill packets are rejected in the first version so an inferior operation cannot
terminate the VM.

## Status and remaining milestones

Completed:

1. Host unit tests cover packet escaping/checksums, task XML, selection, auxv,
   executable identity, breakpoint ownership, probe decoding, and restoration
   on injected transaction failures.
2. A real GDB 17.2 session accepted `pTGID.TID` thread XML from the facade and
   displayed separate process inferiors.  That proof used a host-side oracle;
   it did not enumerate live guest tasks.
3. The AArch64 call gate ran a constant-output self-test in a stopped OpenWrt
   guest, restored byte-identical scratch pages and all saved registers, and
   the same VM subsequently continued through `/init` and `procd`.
4. The read-only task probe, exact-Kbuild build/audit path, stable ABI, decoder,
   and oracle adapter are implemented.  The probe itself has not been compiled
   and run against the downloaded OpenWrt release because its exact configured
   source/generated build tree is not part of the release debug archive.
5. `probe_tool.py callgate-manifest` converts a kernel-bound package plus
   explicit scratch mappings into the strict, atomically published runtime
   manifest.  Scratch mapping discovery/reservation remains manual.
6. `viros.sh inferiors openwrt-arm64 MANIFEST BOOTABLE_KERNEL` validates the
   exact local inputs, reaches `ret_to_user`, transfers sole QEMU ownership to
   the live facade, opens project GDB, and cleans both child processes and all
   project-local sockets on exit and signals.
7. The live facade connects snapshot enumeration and optional checked,
   read-only AArch64 process-memory translation to the multiprocess RSP layer.
   Mock lifecycle and unit tests cover this path.
8. ABI 1.2 reads validated native AArch64 saved EL0 frames for sleeping tasks.
   The facade maps those core values into QEMU's target-description order and
   uses GDB's literal-`x` unavailable encoding for every FP/system register it
   cannot honestly supply. ABI 1.0/1.1 packages remain listing/translation
   compatible and do not advertise this operation.

Remaining:

1. Compile the probe in a user's matching local OpenWrt build and run the wired
   workflow against that exact bootable kernel.  The downloaded official
   release lacks the matching configured/generated Kbuild tree and is not this
   proof.
2. Prove that `info inferiors` lists live init/procd/other guest tasks and that
   selecting sleeping procd reads its ELF header from its own address space.
3. Prove that selecting sleeping procd also yields its saved core register
   frame through real GDB against the exact locally built kernel.
4. Prove that identical virtual-address breakpoints stop only for their owning
   inferior.
5. Port the call gate and register conversion to ARMv7, then MIPS.

Sleeping-task register writes, watchpoints, shared-library enumeration,
fork/exec/exit notifications, compat ABIs, and stepping through a scheduling
syscall are later milestones and must not be implied by the initial process
listing.
