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
runner. The legacy QEMU 5.2 TILE translator is also forced to use this managed
interpreter with its bundled Meson rather than discovering a host `python3`.
Both QEMU builds disable the optional Linux AIO and io_uring backends, which
are unnecessary for this lab and otherwise vary with runner-installed headers.

Target cross compilers do not come from the runner OS either. `download`
fetches checksum-pinned Bootlin toolchains for x86-64, ARM, AArch64, both MIPS
endian variants, and PowerPC. Their host executables are x86-64: an x86-64
Linux runner executes them directly, while an AArch64 Linux runner executes
the identical archives through the `qemu-x86_64` user emulator built by the
QEMU stage. Both host paths have been exercised. The work directory must be on
a case-sensitive filesystem:
Linux has source files whose names differ only by case, so its published patch
cannot be represented correctly on a case-insensitive macOS-backed mount.
`doctor` detects this before a kernel build starts; use a checkout or set
`VIROS_WORKDIR` to a directory on a case-sensitive volume.

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
serial device.
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
process with the same virtual address from causing a false stop. The filter is
implemented without architecture-specific MMU registers: it fingerprints the
selected process through the random bytes addressed by its saved `AT_RANDOM`
entry, then checks that address through QEMU's currently active MMU on every
breakpoint hit. A child immediately after `fork` still has the parent's copied
fingerprint until it executes a new image; distinguishing that case requires
a fuller inferior-aware remote stub.

PIE/ASLR is handled without walking version-specific VMA trees: the command
reads `task->mm->saved_auxv`, uses `AT_PHDR` to calculate the ELF load bias, and
checks the result against `AT_ENTRY`. The executable supplied first must be the
exact guest build and retain its ELF program headers; the optional second file
may be the unstripped executable or its separate debug file. Kernel and
userspace symbols remain loaded together.

The included OpenWrt Malta target provides a reproducible, long-running MIPS
guest for this workflow. It uses OpenWrt's official initramfs kernel, matching
`kernel-debug` archive, and default rootfs; it does not pretend that Malta is a
RouterBOARD hardware model. A complete smoke test is:

```sh
./viros.sh download
./viros.sh build
./viros.sh debug openwrt-malta-le
```

The launcher uses PID 1's `start_thread` to infer its kernel task data, loads
the published BusyBox ELF, and presents the initial GDB prompt at BusyBox's
relocated entry without any guest-side server. The equivalent manual commands
are:

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
