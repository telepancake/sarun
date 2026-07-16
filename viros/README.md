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

RouterOS defaults to the current stable version returned by MikroTik's
`LATEST.7` endpoint. A reproducible run can pin it explicitly:

```sh
ROUTEROS_VERSION=7.23.2 ./viros.sh download
ROUTEROS_VERSION=7.23.2 ./viros.sh extract all
ROUTEROS_VERSION=7.23.2 ./viros.sh debug ppc-e500-smp
```

Useful individual stages and run modes are:

```sh
./viros.sh kernel-debug ppc-440
./viros.sh run ppc-440
./viros.sh debug ppc-440
./viros.sh debug smips
```

`debug` is the single-command source-level workflow: it starts target-specific
QEMU, stops after init has started, connects the Python-enabled multi-target
GDB, and cleans QEMU up when GDB exits. ARM uses a hardware breakpoint at the
symbolic `ret_to_user`; PPC remains stopped at the init-exit panic with
`-no-shutdown`. Before presenting the prompt GDB runs `lx-version`, `lx-ps`,
and examines PID 1 with `$lx_task_by_pid(1)`. MikroTik's printk changes are not
compatible with the stock 5.6.3 `lx-dmesg` helper.

## Strict status matrix

| Target | Emulated machine | Strict status |
| --- | --- | --- |
| `arm` | `virt`, Cortex-A15 | **Success:** PID 1 entered; source-level debug boot can examine it |
| `arm64` | `virt`, Cortex-A57 | **Success:** PID 1 entered; source-level debug boot can examine it |
| `ppc-e500-smp` | `ppce500`, e500v2, one CPU | **Success:** PID 1 entered; source-level debug boot can examine it |
| `ppc-440` | `sam460ex`, 460EX | **Success:** PID 1 entered; source-level debug boot can examine it |
| `smips` | patched Malta, MikroTik `board=vm` | **Success:** PID 1 and its executable were examined; GDB stopped at `/init` entry |
| `mipsbe` | no successful board model yet | **Unfinished:** RB400 hardware path stalls before PID 1 |
| `mmips` | no successful board model yet | **Unfinished:** MT7621 hardware path resets before PID 1 |
| `ppc-e500` | no successful match | **Blocked:** this kernel lacks the QEMU e500 platform path and never reaches PID 1 |
| `ppc-83xx` | none | **Blocked:** QEMU has no matching MPC83xx/RB333/RB600 machine |
| `tile` | disclosed TILE-Gx KVM QEMU | **Blocked:** native TILE-Gx KVM only, no TCG system execution, and incomplete GDB hooks |
| `x86` | `q35`/CHR | **Pending:** strict PID 1 plus Python-GDB validation is not complete |

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

The ARM32 RouterOS image is linked for a raw load offset of `0x48000`; the
QEMU build applies that target-specific load adjustment. Its kernel has no
usable PL011 console, so the translated-block trace supplies the production
boot evidence. The PPC-440 run derives a private Canyonlands device tree with
the `RB1200`, `amcc,canyonlands` compatibility pair required by that kernel.

The three MIPS containers use the disclosed `0x80011000` virtual, physical,
and entry address, avoiding Malta's firmware parameter block. SMIPS can use
MikroTik's disclosed MetaROUTER `board=vm` platform instead of emulating an
RB700. `qemu-mips-routeros.patch` makes Malta pass each kernel argument as a
separate PROM argument, supplies the correct argument count, and provides the
minimal AR9330 UART status mapping used by the panic path. With that platform,
both the production SMIPS kernel and the matching debug rebuild execute
RouterOS `/init`.

MIPSBE does not share that successful path: it enters the RB400 reset loop
before init. MMIPS still faults while accessing absent MT7621 platform/MMIO
state. Neither is presented as working merely because its early kernel code
executes. The embedded `MIPS GENERIC QEMU` string is a CPU PRID label, not
Malta machine support.

Raw ext2 images are created with `mkfs.ext2 -d`, requiring neither root nor
loop devices. They contain the corresponding RouterOS NPK, although successful
embedded debug boots use the extracted initramfs and do not depend on an
invented RouterBOARD flash layout.

## Remaining hardware gaps

The single-core PPC e500 image is distinct from `ppc-e500-smp`: only the SMP
variant contains MikroTik's QEMU-e500 platform support. The 83xx image expects
MPC83xx RouterBOARD hardware absent from upstream QEMU.

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
