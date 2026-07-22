# viros RouterOS boot lab

`viros.sh` downloads pinned QEMU and GDB sources, downloads the current
RouterOS release for every published architecture, extracts its boot payloads,
builds the required emulators and debug kernels, and runs one target at a
time. Everything it creates stays below the directory from which it is run;
the parent repository is not modified.

## Success criterion

A target is a strict success only when both of these have been demonstrated:

1. the kernel actually starts PID 1; and
2. a source-built debug boot reaches PID 1 under GDB and the Linux Python
   extension can examine it.

An `Attempted to kill init` panic after PID 1 has run and exited counts as
proof that init started. A kernel that cannot execute init at all does not.
Extraction, early kernel instructions, a console banner, or a working GDB
stub alone are not success.

## Quick start

```sh
./viros.sh doctor
./viros.sh download
./viros.sh build
./viros.sh extract all
./viros.sh list
./viros.sh debug arm64
./viros.sh debug openwrt-arm
./viros.sh debug openwrt-arm64
```

`doctor` checks only native bootstrap programs needed to build QEMU, GDB, and
Linux (for example `gcc`, `g++`, `make`, flex, bison, and device-tree tools).
The runner's Python installation is not used. Based on the detected host
architecture, `download` fetches a checksum-pinned standalone `uv` and uses it
to install an exact managed CPython below the work directory. QEMU's Meson
bootstrap and the Python-enabled GDB build both use that interpreter and
project-local environments populated from pinned wheels. GDB's GMP and MPFR
prerequisites are likewise built from checksum-pinned source archives below
the work directory, so their development headers need not be installed on the
runner. A pinned project-local Expat build gives GDB the XML target-description
support required for QEMU's ARM and AArch64 register sets. The legacy QEMU 5.2
TILE translator is also forced to use this managed
interpreter with its bundled Meson rather than discovering a host `python3`.
Both QEMU builds disable the optional Linux AIO and io_uring backends, which
are unnecessary for this lab and otherwise vary with runner-installed headers.

Target cross compilers do not come from the runner OS either. `download`
fetches checksum-pinned Bootlin toolchains for x86-64, ARM, AArch64, both MIPS
endian variants, and PowerPC. Their host executables are x86-64: an x86-64
Linux runner executes them directly, while an AArch64 Linux runner executes
the identical archives through the `qemu-x86_64` user emulator built by the
QEMU stage. Both host paths have been exercised.

Linux has source files whose names differ only by case, so its source tree
cannot be represented directly on a case-insensitive macOS-backed mount. The
normal path remains an ordinary out-of-tree build when `VIROS_WORKDIR` is case
sensitive. Otherwise `kernel-debug` uses `bwrap` to mount a short-lived,
case-sensitive tmpfs at an existing path below `VIROS_WORKDIR/build`, unpacks
the already-downloaded pinned Linux source and published MikroTik source update there,
and copies the finished output and provenance back below `VIROS_WORKDIR`.
Nothing is placed in system `/tmp`, and the mount disappears with the build
process. GNU make and compiler temporary files use a `tmp` directory inside
that mount through `TMPDIR`, `TMP`, and `TEMP`. The retained
`build/kernel-TARGET` tree includes generated headers,
scripts, `.config`, `vmlinux`, and a `.viros-case-kbuild` identity record. The
`vmlinux-gdb.py` loader and everything it imports are retained as regular files,
so GDB does not depend on the expired source mount. Before copying the output,
the script also rejects any filenames that would collide on case-insensitive
storage.

This fallback needs `bwrap` and, by default, at least 4 GiB of currently
available RAM plus swap because the uncompressed source and output live in
tmpfs together. `doctor` reports whether the direct path or fallback is ready
before a real build starts. `VIROS_CASE_KBUILD_MIN_KIB` may adjust that
preflight threshold for a known runner configuration; it does not limit the
kernel build's actual memory use.

To compile an out-of-tree Kbuild directory later, `kernel-workspace`
reconstructs the matching published source, verifies it and the retained
configuration and `vmlinux`, reconnects the retained output tree, and runs one
foreground command. The supplied directory is the command's working directory
and the only caller-selected writable path; the reconstructed source and output
disappear when the command exits. For example:

```sh
mkdir -p work/my-module
# Put the module Makefile and sources in work/my-module first.
./viros.sh kernel-workspace arm work/my-module -- \
  sh -c 'make -C "$VIROS_KERNEL_SOURCE" O="$VIROS_KERNEL_OUTPUT" \
    ARCH="$ARCH" CROSS_COMPILE="$CROSS_COMPILE" M="$PWD" modules'
```

The command receives `VIROS_KERNEL_SOURCE`, `VIROS_KERNEL_OUTPUT`, `ARCH`, and
`CROSS_COMPILE`. Tool setup is the other writable project path because an
AArch64 runner may need to create its local adapters for the pinned x86-64
compiler programs. Downloads and the retained Kbuild input are read-only.

RouterOS defaults to the current stable version returned by MikroTik's
`LATEST.7` endpoint. A reproducible run can pin it explicitly:

```sh
ROUTEROS_VERSION=7.23.2 ./viros.sh download
ROUTEROS_VERSION=7.23.2 ./viros.sh extract all
ROUTEROS_VERSION=7.23.2 ./viros.sh debug ppc-e500-smp
```

To use a system NPK already on disk, pass its path explicitly. This bypasses
the downloaded package lookup without copying or renaming the input:

```sh
ROUTEROS_NPK=/path/to/routeros-mmips.npk ./viros.sh debug mmips
ROUTEROS_NPK=/path/to/routeros-mmips.npk ./viros.sh inferiors mmips
```

Useful individual stages and run modes are:

```sh
./viros.sh kernel-debug ppc-440
./viros.sh run ppc-440
./viros.sh debug ppc-440
./viros.sh run mipsbe
./viros.sh debug mipsbe
./viros.sh run mmips
./viros.sh debug mmips
./viros.sh debug smips
./viros.sh run x86
./viros.sh debug x86
./viros.sh run openwrt-malta-le
./viros.sh debug openwrt-malta-le
./viros.sh debug openwrt-arm
./viros.sh debug openwrt-arm64
```

`debug` is the single-command source-level workflow: it starts target-specific
QEMU, stops after init has started, connects the Python-enabled multi-target
GDB, and cleans QEMU up when GDB exits. `gdb <target>` is an alias for this
complete workflow; supplying an explicit remote as the second argument keeps
the attach-only form, for example `gdb mmips :1234`. ARM uses a hardware
breakpoint at the symbolic `ret_to_user`; MIPS stops at `start_thread`; and x86
stops at `compat_start_thread` because its RouterOS init is IA32. MIPS and x86
inspect PID 1 and then stop at the extracted `/init` entry. PPC remains stopped
at the init-exit panic with `-no-shutdown`. Before presenting the prompt GDB runs
`lx-version`, `lx-ps`, and examines PID 1 with `$lx_task_by_pid(1)`. MikroTik's
printk changes are not compatible with the stock 5.6.3 `lx-dmesg` helper.
The workflow prints QEMU's PID, GDB socket, and log paths before starting GDB.
At the GDB prompt, `viros-console` resumes the VM and attaches the current
terminal to its serial console. On first use it replays the retained boot output
before showing new live output. Press `Ctrl-]` to stop the VM and return to GDB;
the harness breakpoints are temporary, so they do not immediately catch again
when the console resumes. Console output is also retained in the target's
`debug-console.log`. QEMU runs in a separate terminal session, so `Ctrl-C`
while GDB is continuing interrupts the remote target instead of terminating
the VM process. ARM's QEMU `virt` machine uses the PL011 console
`ttyAMA0`; the other kernel command lines select their target's corresponding
serial device. A launcher watchdog also follows QEMU's lifetime: if the remote
VM disappears while GDB is still attached, the associated GDB is terminated
(and forcibly reaped after a short grace period) instead of being left in a
remote-event CPU loop.
You can inspect the retained output without resuming the VM as well:

```sh
less artifacts/arm/debug-console.log
tail -f artifacts/arm/debug-console.log
```

Ordinary `run` sessions route the supported machine's serial device to the
calling terminal. TILE has no TCG system emulation, and `ppc-83xx` still has no
matching QEMU machine, so neither can provide a usable emulated console.
For MMIPS, the emulated MT7621 UART is routed to
`artifacts/mmips/debug-console.log`. It can remain quiet while GDB holds the VM
at `/init`, then shows kernel diagnostics once `viros-console` resumes it;
RouterOS init does not necessarily provide an interactive shell on that UART.

### Userspace debugging through QEMU

QEMU's system GDB stub can debug a guest userspace process without running
`gdbserver` inside the guest. QEMU exposes CPUs rather than Linux processes, so
viros adds the missing kernel-aware process selection and ELF relocation. With
the matching kernel symbols loaded, select an existing PID or the 15-character
Linux `comm`, then provide the local executable and optional separate debug
file:

```gdb
viros-user-debug procd /openwrt/build_dir/.../procd /openwrt/debug/procd.debug
viros-user-info
viros-user-break main
continue
bt
```

For a process already stopped at its entry point, `viros-user-load PID EXEC
[DEBUG-FILE]` skips the scheduler wait. `viros-user-focus PID|COMM` waits at
`finish_task_switch` until that task owns a QEMU CPU. `viros-user-break` and
`viros-user-tbreak` filter breakpoint hits by Linux PID, which prevents another
process with the same virtual address from causing a false stop. The filter
fingerprints the selected process through the random bytes addressed by its
saved `AT_RANDOM` entry, then checks that address through QEMU's currently
active MMU on every breakpoint hit. When AArch64 QEMU cannot translate the EL0
stack at the kernel return boundary, the extension uses the normalized
`TTBR0_EL1` translation-table identity until the cookie becomes readable at
the first userspace instruction. A child immediately after `fork` still has the parent's copied
fingerprint until it executes a new image; distinguishing that case requires
a fuller inferior-aware remote stub.

PIE/ASLR is handled without walking version-specific VMA trees: the command
reads `task->mm->saved_auxv`, uses `AT_PHDR` to calculate the ELF load bias, and
checks the result against `AT_ENTRY`. The executable supplied first must be the
exact guest build and retain its ELF program headers; the optional second file
may be the unstripped executable or its separate debug file. Kernel and
userspace symbols remain loaded together.

The included OpenWrt Malta, ARMv7, and AArch64 targets provide reproducible,
long-running guests for this workflow. They use OpenWrt's official initramfs
kernels, matching `kernel-debug` archives, and root filesystems. Malta is a
MIPS regression target rather than a claimed RouterBOARD hardware model. A
complete smoke test for any of the three native ABIs is:

```sh
./viros.sh download
./viros.sh build
./viros.sh debug openwrt-malta-le
./viros.sh debug openwrt-arm
./viros.sh debug openwrt-arm64
```

The launcher stops at the architecture's exact return-to-userspace path, uses
PID 1 to infer its kernel task data, loads the published BusyBox ELF, and
presents the initial GDB prompt at BusyBox's relocated entry without any
guest-side server. Substitute `openwrt-arm` or `openwrt-arm64` in the paths
below for those targets. The equivalent manual commands are:

```gdb
viros-user-load 1 artifacts/openwrt-malta-le/rootfs-25.12.5/bin/busybox
viros-user-tbreak entry
continue
viros-console
```

`viros-console` boots through `procd`; press `Ctrl-]` to return to GDB, then:

```gdb
viros-ps
viros-user-debug procd artifacts/openwrt-malta-le/rootfs-25.12.5/sbin/procd /path/to/matching/unstripped/procd
viros-user-break main
continue
```

The release rootfs's `procd` is stripped and has no ELF section table. Passing
it without the optional debug ELF still provides PIE relocation, PID filtering,
and numeric breakpoints; source names require an exact unstripped or separate
debug ELF from the corresponding OpenWrt build. The official OpenWrt kernel
debug archive also uses reduced DWARF and leaves `task_struct` incomplete, so
the extension infers the required task-list, PID, command-name, `mm`, and saved
aux-vector offsets from the live `init_task` and PID 1. This is why `viros-ps`
works here even though the stock Linux helpers cannot enumerate tasks.

This is most reliable when the kernel and userspace have the same register ABI.
Compat processes (such as an IA32 executable under an x86-64 kernel) can be
selected and relocated, but register decoding and unwinding depend on QEMU's
compat-mode target description. Shared-library symbols are not loaded
automatically yet; the main executable, static programs, and pre-libc failures
are the intended first use case.

### Experimental Linux-process inferiors

There is also an experimental successor to `gdb_user.py` under
`inferiors/`, `probe/`, and `callgate/`. Its intended boundary is a
multiprocess GDB remote facade backed by a small probe compiled through the
exact guest kernel's configured Kbuild tree. Unlike `viros-user-*`, that
facade presents the probe's frozen Linux task snapshot as actual GDB
inferiors. The complete live task path has been exercised with viros' matching
MMIPS and x86 kernel builds. The external AArch64/OpenWrt path is implemented,
but this is not a claim that the downloaded OpenWrt release kernel was probed
with an exact separately configured Kbuild tree.

#### Integrating a locally built kernel

The reusable integration supports x86-64, little-endian ARMv7 in ARM state,
AArch64, and little-endian MIPS32r2/o32 (`mmips`). It requires all of the
following from one kernel build:

- the configured source tree and generated Kbuild output which produced the
  guest kernel;
- its matching cross compiler and binutils;
- the unstripped `vmlinux`, with full debug information and one GNU build ID;
- the bootable `bzImage`, `zImage`, `Image`, or ELF produced by that same
  build; and
- a QEMU machine which can boot that image and expose its CPUs and memory
  through the GDB remote protocol.

Do not combine an official boot image with a conveniently similar `vmlinux`,
or a debug archive's `vmlinux` with a separately configured source tree. The
probe object, package, scratch map, Linux Python helpers, and running boot
image must describe the same build. The package tools bind these artifacts by
SHA-256 and GNU build ID, but the runtime does not independently recover the
build ID from guest memory.

Install the reserved workspace and event observer before configuring and
building the kernel. The helper is idempotent, so it is safe to run again when
updating an existing build tree. Pass the configuration which will actually be
used for the build when one already exists (including an out-of-tree
configuration):

```sh
./viros.sh kernel-support /path/to/linux /path/to/output/.config
```

This modifies a prepared source tree; it does not rewrite an already-built
compressed kernel image. Rebuild `vmlinux` and its boot image afterward. A
post-build binary edit cannot safely add the reserved executable/data pages,
the event observer, matching DWARF, and the kernel's page-permission setup.

Omit the final argument if the kernel has not been configured yet. In that
case, enable `CONFIG_TRACEPOINTS=y`, `CONFIG_DEBUG_INFO=y`, and
`CONFIG_GDB_SCRIPTS=y` in the configuration before building. The command
copies the versioned support sources into `kernel/viros`, adds that directory
to `kernel/Makefile`, and adds this guarded link anchor to the top-level
Makefile:

```make
ifeq ($(VIROS_SCRATCH),1)
KBUILD_LDFLAGS += --undefined=__viros_scratch_code_start
KBUILD_LDFLAGS += --undefined=__viros_scratch_data_start
KBUILD_LDFLAGS += --undefined=__viros_scratch_stack_start
endif
```

When given a configuration, `kernel-support` enables those three options and
disables reduced and split debug information using the kernel's own
`scripts/config`. Preserve the kernel's normal architecture, ABI, page-size,
endianness, and machine configuration. Pass both integration variables on
every make invocation which can relink `vmlinux` or the boot image:

```sh
make -C linux O="$KERNEL_OUTPUT" ARCH="$ARCH" \
  CROSS_COMPILE="$CROSS_COMPILE" \
  VIROS_SCRATCH=1 VIROS_EVENTS=1 olddefconfig
make -C linux O="$KERNEL_OUTPUT" ARCH="$ARCH" \
  CROSS_COMPILE="$CROSS_COMPILE" \
  VIROS_SCRATCH=1 VIROS_EVENTS=1 -j"$JOBS" \
  vmlinux scripts_gdb "$BOOT_TARGET"
```

Use `ARCH=x86_64`, `arm`, `arm64`, or `mips` as appropriate;
`BOOT_TARGET` is normally `bzImage`, `zImage`, or `Image`, and may be omitted
when QEMU directly boots the linked ELF. Confirm that the result contains the
six `__viros_scratch_*` boundary symbols and `viros_event_stop`. The boot image
passed to QEMU must come from this output directory. An initramfs or disk may
come from the firmware build being examined, provided it is compatible with
that exact kernel.

Package that exact output for another runner with `kernel-bundle`. The
unstripped symbol file defaults to `KBUILD_OUTPUT/vmlinux`. The boot image
defaults to `arch/x86/boot/bzImage`, `arch/arm/boot/zImage`,
`arch/arm64/boot/Image`, or `vmlinux` for the fixed MMIPS Malta profile.

Compiler, final linker, objcopy, and make default only when the exact Kbuild
`.cmd` records contain one unambiguous executable. A bare recorded program is
resolved through the captured build box's `PATH`; ViroS never invents an SDK
directory. GNU compiler/linker/objcopy names can also establish the exact
`CROSS_COMPILE` prefix. Kbuild commonly does not retain the top-level make
executable, so `--make` is normally the one required tool argument. If any
identity is absent, unavailable, or ambiguous, publication stops and names the
specific explicit option needed.

For example, when GNU tools are fully recorded, MMIPS publication is normally:

```sh
./viros.sh kernel-bundle \
  --arch mmips \
  --kbuild-output /path/to/output \
  --output-dir artifacts/my-kernel.bundle \
  --make /path/to/sdk/bin/make
```

Repeat the same Kbuild assignments used for the kernel itself. LLVM tool names
do not encode `CROSS_COMPILE`, so an LLVM 21 AArch64 build can be packaged with:

```sh
./viros.sh kernel-bundle \
  --arch aarch64 \
  --kbuild-output /path/to/output \
  --output-dir artifacts/my-kernel.bundle \
  --cross-compile aarch64-openwrt-linux-musl- \
  --make /path/to/sdk/bin/make \
  --make-arg LLVM=-21 --make-arg LLVM_IAS=1 \
  --code-gpa 0x... --data-gpa 0x... --stack-gpa 0x...
```

If that Kbuild output did not retain one of the LLVM executable identities,
add the corresponding `--compiler`, `--cross-ld`, or `--objcopy` option shown
by the error.

For a GNU build whose command records are incomplete, pass its exact
`...-gcc`, `...-ld`, `...-objcopy`, and `CROSS_COMPILE` prefix. An explicitly
empty `CROSS_COMPILE` is written as `--cross-compile ''`. The bundler appends
the resolved compiler as an absolute `CC=...` assignment for the helper build,
so a same-named host compiler cannot take its place. X86-64, ARMv7, and
AArch64 require the three physical addresses from the Sarun/QEMU machine that
will run the kernel. MMIPS derives its mappings from KSEG0 and rejects those
three options. `--runtime-offset` applies one measured KASLR offset to both the
reserved regions and `init_task`; leave it at zero for a non-relocated kernel.

The resulting directory contains the boot image, unstripped `vmlinux`, exact
configuration, Linux GDB Python helpers, scratch description, built and linked
probe, call-gate manifest, and build logs. `bundle.json` records the kernel
build ID, every bundled file's SHA-256 and size, the original input paths, the
exact cross prefix and Kbuild assignments, and the resolved path, SHA-256, and
version of every selected tool. The call-gate uses relative paths, so the whole
directory can be copied to another system without rewriting it.

When Sarun produces this bundle, run the helper build in a network-disabled,
read-only child of the recorded kernel-build box. Reuse the captured build cwd,
environment, Kbuild output, cross prefix, assignments, and tool hashes. A
userspace-only SDK that merely targets the same architecture cannot establish
that the external helper matches the configured kernel build.

### Install the managed Sarun debugger provider

After `download` and `build`, publish the debugger implementation as one named
Sarun box:

```sh
./viros.sh sarun-install
```

This records the `VIROS-DEBUG` box as the sole provider of the `viros-debug`
service, with fixed provider and GDB-client entry points. It copies the managed
GDB and Python runtime into that box and does not record paths to the checkout.
There are no provider, GDB, Python, session-name, symbol, or source-path options
on the runtime command.

The provider box needs to be republished after rebuilding GDB or updating the
viros Python modules. GDB uses a provider-relative Python-library runpath, so a
captured provider remains usable when the Sarun store is moved to another host
of the same architecture.

### Publish a complete image from the build box

Run `image-bundle` in the same named Sarun box that built the root filesystem,
kernel bundle, and unstripped userspace programs. It emits one deterministic
initramfs and one `viros-image-bundle-v1` manifest. For example:

```sh
./viros.sh image-bundle \
  --arch aarch64 \
  --rootfs bin/targets/armsr/armv8/rootfs \
  --kernel-bundle artifacts/openwrt-kernel.bundle \
  --output-dir artifacts/openwrt-debug.image
```

By default, the publisher examines every regular executable ELF in the rootfs
and finds its unstripped counterpart by architecture and GNU build ID. It
searches a bounded, deterministic set consisting of the captured current build
tree and the rootfs, output, and kernel-bundle parents; the rootfs, output, and
kernel bundle themselves are excluded. Programs without a matching DWARF ELF
are simply left uncataloged. Multiple byte-distinct DWARF ELFs claiming one
needed build ID are rejected instead of choosing by pathname.

`--executable GUEST=ELF` remains available for an exceptional exact
association. Explicit and discovered associations are combined, with an
explicit guest path considered once. The runtime file is read from `--rootfs`;
the second file must be its matching unstripped ELF with DWARF. Publication
refuses a build-ID mismatch, wrong ELF architecture, absent DWARF line
information, duplicate explicit guest path, or ambiguous build ID. It copies
the debugger ELF under its build ID, hashes every published artifact, embeds
the exact kernel bundle, and generates the initramfs itself instead of
depending on a host `cpio`.

Run `image-bundle` inside the same captured build box that produced those ELF
files. The provider receives that box as an attachment, which is what makes the
absolute compilation directories recorded in DWARF resolve to the captured
source files automatically; publishing elsewhere cannot reconstruct that
source identity.

These arguments are paths inside the build box, not runtime resource-selection
options. After capture, Sarun finds `image.json` and its nested `bundle.json`
through the selected box's normal parent/attachment chain. The image declares
one versioned boot profile; it cannot inject arbitrary QEMU arguments. The
`viros-debug` provider receives the build box as a read-only attachment, so the
DWARF compilation paths resolve against the immutable captured source tree at
the same paths used by the compiler.

With the Sarun TUI running, select the exact captured boot payload. A combined
firmware file uses `--image`; a direct Linux boot selects both QEMU inputs:

```sh
sarun run --net off --qemu aarch64 --debug \
  --kernel OPENWRT-BUILD bin/targets/.../Image \
  --initramfs OPENWRT-BUILD bin/targets/.../rootfs.cpio.gz
```

The box selectors and captured relative paths identify only the boot payload;
there is no host path, symbol path, sysroot path, source-map, debugger, or raw
QEMU option. Sarun rejects a partial pair or a pair combined with `--image`,
revalidates both immutable identities before launch, and passes the exact bytes
to QEMU as sealed descriptors. Sarun opens the managed GDB terminal
automatically after the debug facade is ready. Kernel symbols are already
loaded; each cataloged userspace process appears as a GDB inferior and receives
its exact build-ID-checked debug ELF when it is first observed. Missing,
changed, mismatched, or ambiguous debugger resources are an error.

After the build, `probe/scratch/scratch_tool.py` records the reserved regions
from `vmlinux`; `probe/probe_tool.py build` compiles the transient snapshot
code through the same configured Kbuild output. Package it at the recorded
code address, then create the call-gate manifest as shown below. For KASLR,
pass the measured runtime relocation to `scratch_tool.py`; otherwise disable
KASLR or use the default zero offset. AArch64, ARMv7, and x86-64 also require
the three current guest physical mappings obtained from the stopped VM.
MMIPS mappings must remain in KSEG0 and are derived from their virtual
addresses.

The two kernel-side pieces have deliberately different lifetimes:

- `viros_scratch.S` only reserves code, data, and stack pages. During a frozen
  transaction the host saves those pages and CPU registers, places the
  audited snapshot executable in the code page, runs it to its completion
  boundary, reads the result, and restores every byte and register. The
  executable has no module entry point, kernel thread, exported API, or
  normal boot-time path.
- `viros_event.c` is a persistent built-in observer initialized by a
  `core_initcall`. It keeps one preallocated record per CPU and observes the
  signal-delivery and kernel-die boundaries without allocation or logging.
  A facade-owned breakpoint at `viros_event_stop` turns a supported event
  into a GDB stop with the saved userspace register frame.

The external three-argument launcher currently provided by the shell script
is the AArch64 form:

```sh
./viros.sh inferiors openwrt-arm64 CALLGATE.json BOOTABLE_KERNEL
```

The x86, ARMv7, AArch64, and MMIPS RouterOS targets use the same components
through their project-managed `kernel-debug`/`inferiors` path. A different
external board or QEMU machine needs an equivalent launcher supplying its
machine arguments, console, initramfs or disk, and initial stopped-kernel
boundary; the manifest and facade formats themselves are not tied to
OpenWrt.

The following live boundaries have been exercised:

- stock GDB 17.2 recognized synthetic `pTGID.TID` identities from the facade
  as separate inferiors; this test used a host-side test oracle, not live guest
  tasks; and
- the AArch64 call gate injected a small self-test into a stopped OpenWrt
  guest, restored all scratch pages and registers byte-for-byte, and then let
  that same VM continue through `/init` and `procd`;
- the MMIPS facade enumerated live PID 1 as a GDB inferior and restored the
  helper transaction, including the writable fields of CP0 Cause; and
- the x86 facade entered the helper from a stopped IA32 `/init`, restored its
  userspace segments and GS bases, exposed PID 1, and ran the matching
  kernel's `lx-ps` command successfully.

At x86 `/init`'s very first entry stop, its file-backed text page can still be
demand-paged: QEMU may report that instruction memory unavailable until the
guest takes the normal first page fault. Continuing proved that `/init` then
runs and prints its startup messages. This does not affect kernel memory or
`lx-ps`, and a later process breakpoint sees resident text.

The exact-Kbuild task probe, its stable response ABI, and the host decoder are
implemented and audited by unit tests. They were not compiled and run against
the downloaded OpenWrt release here: the release debug archive contains
`vmlinux`, but not its exact configured source/generated Kbuild tree. A local
OpenWrt build provides that missing input. In particular,
`build/probe-arm64-kbuild/source` is a Linux 5.6.3 portability build, while the
tested official OpenWrt debug image identifies a Linux 6.12.94 build directory;
those artifacts must not be combined. Given one matching local build, compile and
absolute-link the probe with the matching tools as follows (the scratch code
address is deliberately supplied by the caller):

```sh
python3 probe/probe_tool.py build \
  --linux-dir /path/to/openwrt/build_dir/target-*/linux-*/linux-* \
  --output-dir build/probe-openwrt-arm64 \
  --arch aarch64 \
  --cross-compile /path/to/openwrt/staging_dir/toolchain-*/bin/aarch64-openwrt-linux-musl- \
  --vmlinux /path/to/matching/vmlinux

python3 probe/probe_tool.py package \
  build/probe-openwrt-arm64/probe.json \
  --load-address 0xffff800000000000 \
  --output-dir build/probe-openwrt-arm64/package \
  --cross-ld /path/to/exact/aarch64-openwrt-linux-musl-ld \
  --objcopy /path/to/exact/aarch64-openwrt-linux-musl-objcopy
```

Replace the example load address with an unused, mapped, page-aligned kernel
scratch region reserved for the guest being debugged. Do not copy it verbatim.
The package command deliberately accepts the `probe.json` build record, not a
bare object file. The build record binds the object to the matching kernel's
SHA-256 and GNU build ID; those identities are carried into `package.json`.

Once three unused scratch mappings have been reserved, generate the strict
runtime manifest from those explicit mappings:

```sh
python3 probe/probe_tool.py callgate-manifest \
  build/probe-openwrt-arm64/package/package.json \
  --vmlinux /path/to/matching/vmlinux \
  --output build/probe-openwrt-arm64/callgate.json \
  --code-gva 0xffff800000000000 --code-gpa 0x40000000 --code-size 0x1000 \
  --data-gva 0xffff800000001000 --data-gpa 0x40001000 --data-size 0x10000 \
  --stack-gva 0xffff800000011000 --stack-gpa 0x40011000 --stack-size 0x1000 \
  --cpu 0 --init-task 0xffff800081234000
```

Every address in that command is an example, including `init_task`; obtain the
real values from the exact stopped guest and matching symbols. The three
regions must be distinct, page-aligned, no larger than 64 KiB, mapped at the
declared physical addresses on the selected CPU, and genuinely unused. The
packaged load address must equal `--code-gva`. The command verifies that
`--vmlinux` has the SHA-256 and build ID carried from the exact-Kbuild stage,
then validates and atomically publishes the manifest using a temporary file
beside `--output`. Mapping discovery and reservation remain intentionally
manual.

`./viros.sh debug openwrt-arm64` and the attach form
`./viros.sh gdb openwrt-arm64 REMOTE` now load the experimental commands:

```gdb
viros-probe-plan build/probe-openwrt-arm64/callgate.json
viros-probe-run build/probe-openwrt-arm64/callgate.json
```

`viros-probe-plan` validates the manifest and shows the transaction without
changing the guest. `viros-probe-run` performs the reversible AArch64
transaction through GDB's Python API. That interactive backend cannot impose a
wall-clock or instruction-budget timeout; if the injected probe does not reach
its completion breakpoint, interrupt GDB with `Ctrl-C` so the transaction's
cleanup restores the saved state. The live-facade launcher below instead owns
QEMU's RSP socket directly and enforces `timeout_seconds` as a restoring host
wall-clock timeout. Neither backend supplies an emulated-instruction budget.

The runtime verifies that GDB loaded the exact manifest-bound `vmlinux` file
and that QEMU reports every declared virtual-to-physical mapping. It does not
yet independently read the running guest's build ID from memory, so use this
only with a guest known to have booted that same kernel image.

For an AArch64 initramfs kernel produced by that same local build, the complete
launcher is:

```sh
./viros.sh inferiors openwrt-arm64 \
  build/probe-openwrt-arm64/callgate.json \
  /path/to/the-matching-openwrt-initramfs-kernel.bin
```

For MMIPS, `kernel-debug` installs the built-in KSEG0 workspace and produces
the exact-Kbuild snapshot package and portable manifest automatically:

```sh
./viros.sh kernel-debug mmips
./viros.sh inferiors mmips
```

The retained inputs are below `artifacts/mmips/inferiors/`. The launcher uses
the same Malta/34Kf/MT7621 shape as `debug mmips`, proves that the kernel has
started `/init`, then opens GDB on the snapshot facade with `info inferiors`
already displayed. MMIPS provides task enumeration plus the stopped current
CPU's registers and virtual-memory view. This is sufficient for the matching
kernel's stock `lx-ps` helper and for examining the current `/init` process.
Its package deliberately does not advertise saved registers or address-space
translation for sleeping tasks, so those reads fail explicitly.

At the initial GDB prompt, the current task is PID 1 and the matching Linux
helpers are already loaded:

```gdb
info inferiors
lx-ps
viros-console
```

There is deliberately no default to the downloaded official OpenWrt image.
The command validates the manifest-bound probe and `vmlinux` before starting
anything, boots the supplied kernel stopped, uses the exact `vmlinux` in a
short noninteractive GDB session to reach `ret_to_user`, and disconnects while
the VM remains stopped. The live facade then becomes QEMU's sole RSP client,
runs the restoring snapshot transaction, opens its own socket, and starts the
project GDB against that socket. In GDB:

```gdb
info inferiors
inferior 2
viros-console
```

`viros-console` replays retained output and resumes the VM; `Ctrl-]` stops it
and returns to GDB. Per-run logs are retained below
`artifacts/openwrt-arm64/inferiors-PID/`. Both Unix sockets are below `build/`,
and the launcher removes them and terminates/reaps QEMU and the facade on
normal exit, failure, `SIGINT`, or `SIGTERM`.

The live facade now refreshes task snapshots, exposes their multiprocess IDs,
and, when an AArch64 sealed probe advertises `translate-va-aarch64-v1`, supplies
read-only selected-process virtual memory through checked page translation.
For a current task it returns QEMU's stopped-vCPU register block. When the
sealed ABI 1.2 package also advertises `saved-regs-aarch64-v1`, a sleeping
native AArch64 task instead gets its validated saved EL0 x0-x30, SP, PC, and
PSTATE frame; GDB receives literal `x` digits for unavailable FP/system
registers rather than invented values. ARMv7, x86-64, and MMIPS packages
currently advertise task snapshots only; sleeping-task registers and
selected-process address translation therefore fail explicitly on those
targets. Saved AArch32 frames under AArch64, register and process-memory
writes, automatic userspace ELF symbol loading, and the other MIPS variants
remain unimplemented. The built-in observer records and presents both
default-fatal userspace signal delivery and kernel `DIE_OOPS`; it is not a
general syscall or scheduler event stream. A panic which does not pass through
`DIE_OOPS` is not synthesized because the panic notifier supplies no saved
register frame. Native ARMv7, native AArch64, MIPS32, native x86-64, and x86
IA32 signal frames are represented, while AArch32 events under an AArch64
kernel are withheld. The facade's direct RSP call gate has a restoring
wall-clock timeout, but it is not an emulated instruction budget and ordinary
QEMU virtual time can advance during the transaction.

The launcher and facade have lifecycle/unit coverage, but this repository has
not run the exact-Kbuild probe against a matching locally built OpenWrt guest;
that live proof requires the user's kernel build and manifest. The detailed
invariants and remaining proof milestones are in
[DESIGN-inferiors.md](DESIGN-inferiors.md).

## Strict status matrix

| Target | Emulated machine | Strict status |
| --- | --- | --- |
| `arm` | `virt`, Cortex-A15 | **Success:** PID 1 entered; source-level debug boot can examine it |
| `arm64` | `virt`, Cortex-A57 | **Success:** PID 1 entered; source-level debug boot can examine it |
| `ppc-e500-smp` | `ppce500`, e500v2, one CPU | **Success:** PID 1 entered; source-level debug boot can examine it |
| `ppc-e500` | `ppce500`, e500v2, RB1000 DTB | **Success:** PID 1 entered; source-level debug boot can examine it |
| `ppc-440` | `sam460ex`, 460EX | **Success:** PID 1 entered; source-level debug boot can examine it |
| `smips` | patched Malta, MikroTik `board=vm` | **Success:** PID 1 and its executable were examined; GDB stopped at `/init` entry |
| `mipsbe` | patched Malta, MikroTik `board=vm` | **Success:** PID 1 and its executable were examined; GDB stopped at `/init` entry |
| `x86` | PC/CHR disk | **Success:** production CHR reached login; rebuilt kernel started PID 1 and GDB stopped at `/init` entry |
| `mmips` | patched Malta, 34Kf with MT7621 compatibility | **Success:** PID 1 and its executable were examined; GDB stopped at `/init` entry |
| `openwrt-malta-le` | Malta, 24Kc | **Test target success:** official OpenWrt reached `/init` and `procd`; reduced-DWARF task enumeration, PIE relocation, PID-filtered breakpoints, and console switching were exercised |
| `openwrt-arm` | `virt`, Cortex-A15 | **Test target success:** official ARMv7 OpenWrt reached `/init` and `procd`; GDB stopped at BusyBox's ELF entry and exercised process-aware PIE debugging and console switching |
| `openwrt-arm64` | `virt`, Cortex-A57 | **Test target success:** official AArch64 OpenWrt reached `/init` and `procd`; GDB stopped at BusyBox's ELF entry and exercised process-aware PIE debugging and console switching |
| `ppc-83xx` | none | **Blocked:** QEMU has no matching MPC83xx/RB333/RB600 machine |
| `tile` | disclosed TILE-Gx KVM QEMU | **Blocked:** native TILE-Gx KVM only, no TCG system execution, and incomplete GDB hooks |

`./viros.sh list` prints the corresponding concise status from the script.

## What `debug` boots

The production RouterOS kernels are stripped images, so a separately built
debug kernel is required for reliable types, symbols, and Linux GDB helpers.
The download records an immutable revision of the public mirror of MikroTik's
GPL disclosure. For each supported debug target, the build uses:

- the official Linux 5.6.3 base tree;
- MikroTik's disclosed `linux-5.6.3.patch`;
- MikroTik's target-specific published configuration, with the debug and GDB
  script options enabled; and
- the initramfs extracted from the selected current RouterOS package.

`debug` boots that rebuilt kernel with the current extracted RouterOS
initramfs. GDB opens the `vmlinux` from that same kernel output directory and
sources `build/kernel-<target>/vmlinux-gdb.py`, generated in that exact output
tree. This keeps the running debug kernel, DWARF types, symbols,
configuration, and Python extension consistent with one another. Sourcing the
similarly named script from the source tree would give it the wrong Python
module search path.

This workflow does **not** claim that the rebuilt debug `vmlinux` is
bit-identical to the latest stripped production RouterOS kernel. It uses the
published kernel version, MikroTik patch, and configuration to provide the
traceable companion boot required by the success criterion; `run` remains the
path for the extracted production boot payload.

## Extraction details

For embedded architectures, `boot/kernel` is an ELF decompression wrapper
whose compressed Linux payload is stored outside its loadable program
segment. Passing that wrapper directly to QEMU therefore omits bytes the
wrapper needs. `npk_extract.py` reads the NPK archive, splits its aligned XZ
streams into the Linux image and CPIO initramfs, preserves and classifies all
four PPC variants, and emits the minimal ELF containers required by QEMU's
direct kernel loaders.

The x86 system NPK instead carries its Linux bzImage as `BOOTX64.EFI`. The
extractor decompresses the bzImage's XZ Linux ELF and recovers its embedded
newc initramfs and IA32 `/init`. A normal `run x86` boots the official CHR disk
to its login prompt. The strict debug path directly boots the matching x86-64
rebuild with that extracted production initramfs and the CHR disk on the
emulated PIIX IDE controller. The download stage supplies its x86-64 target
compiler, and on a non-x86-64 host the build stage runs that compiler through
its user-mode QEMU adapter.

The ARM32 RouterOS image is linked for a raw load offset of `0x48000`; the
QEMU build applies that target-specific load adjustment. Its kernel has no
usable PL011 console, so the translated-block trace supplies the production
boot evidence. The PPC-440 run derives a private Canyonlands device tree with
the `RB1200`, `amcc,canyonlands` compatibility pair required by that kernel.

The three MIPS containers use the disclosed `0x80011000` virtual, physical,
and entry address, avoiding Malta's firmware parameter block. MIPSBE and SMIPS
can use MikroTik's disclosed MetaROUTER `board=vm` platform instead of
emulating their RouterBOARD hardware. `qemu-mips-routeros.patch` makes Malta
pass each kernel argument as a separate PROM argument, supplies the correct
argument count, and provides the minimal AR9330 UART status mapping used by
the panic path.

MIPSBE's published configuration uses `CONFIG_MAPPED_KERNEL` and maps RAM at
`0xc0000000`. QEMU places a Malta initrd at a dynamically chosen, 64-KiB-aligned
physical offset; the script mirrors that placement calculation from the
actual initramfs size and passes the corresponding mapped `rd_start` plus
`rd_size`. With those target-specific details, both production kernels and
their matching debug rebuilds execute RouterOS `/init`.

MMIPS selects the disclosed `board=750g-mt` path. The QEMU patch keeps the MIPS
coherence manager present with one active 34Kf CPU and supplies the polling
MT7621 UART status register at `0x1e000c00`; this is the hardware shape needed
for that kernel to finish early platform setup. The 34Kf model also matches
the production init's legacy-NaN MIPS32r2 ABI. QEMU's initial GDB stop reply
includes the CPS thread ID so upstream GDB can retain the selected CPU. With
those compatibility pieces, both the extracted production kernel and the
matching `mmips.config` rebuild enter `/init`; the debug path examines PID 1
with the exact output-tree Linux Python helpers and stops at the extracted
entry address.

Raw ext2 images are created with `mkfs.ext2 -d`, requiring neither root nor
loop devices. They contain the corresponding RouterOS NPK, although successful
embedded debug boots use the extracted initramfs and do not depend on an
invented RouterBOARD flash layout.

## Remaining hardware gaps

The single-core PPC e500 kernel selects its disclosed RB1000 platform through
the root device-tree compatibility string. The script asks QEMU to generate a
fresh ppce500 tree for the exact kernel/initramfs pair, preserving its dynamic
load addresses, then changes only the root model and compatibility. The kernel
also uses SPR 1023 for a private `hv_yield` call; `qemu-ppc-routeros.patch`
implements call 16 with the kernel's CR0.SO success convention. Both the
production image and the matching `e500.config` debug rebuild then reach
`/init`. The 83xx image expects MPC83xx RouterBOARD hardware absent from
upstream QEMU.

MikroTik's disclosure also contains a patched QEMU 2.0.2
`tilegx-softmmu`. It is not portable TILE system emulation: it requires a
native TILE-Gx host with KVM, has no TCG execution path, and leaves GDB register
and guest-memory support unfinished. The extractor exposes its inner TILE-Gx
Linux ELF and initramfs, but doing so cannot satisfy the traceable PID 1
criterion on current non-TILE hosts.

QEMU 5.2's separate TILE-Gx linux-user translator was also exercised directly
against the extracted RouterOS init. It entered the executable and performed
initial syscalls, then stopped on an unimplemented TILE opcode. That is useful
translator evidence, but it runs neither the MikroTik kernel nor its hardware
platform and therefore is not a TILE boot or a success under this project's
criterion.
