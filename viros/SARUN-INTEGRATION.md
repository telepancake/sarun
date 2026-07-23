# ViroS integration with Sarun

Sarun can run captured Linux firmware boot artifacts as a managed QEMU
debugging session. Select one captured initramfs or supported combined firmware
artifact:

```sh
sarun run --net off --qemu aarch64 --debug \
  --image OPENWRT-BUILD bin/targets/.../rootfs.cpio.gz
```

or select the exact two files accepted by QEMU's direct Linux boot form:

```sh
sarun run --net off --qemu aarch64 --debug \
  --kernel OPENWRT-BUILD bin/targets/.../Image \
  --initramfs OPENWRT-BUILD bin/targets/.../rootfs.cpio.gz
```

`OPENWRT-BUILD` is a Sarun box selector and both following paths are captured,
box-relative paths. They are the only explicit payload selection. There are no
host-path, debugger, Python, symbol, source, socket, sysroot, toolchain, or raw
QEMU-argument options. The kernel and initramfs form is a closed pair: neither
half is accepted alone or together with `--image`.

Sarun resolves the files from their owning captured layers and uses the
matching prevalidated kernel debugger bundle and source already published in
that captured context, together with the named `viros-debug` provider from
`VIROS-DEBUG`. It rechecks size and SHA-256 immediately before handoff. If the
single selected file already matches a published image-bundle initramfs, that
bundle is used directly. Otherwise the engine sends the exact selected
identity, its finite build-record ancestry, and the resolved kernel bundle to
the provider through a private service-box capsule. The provider extracts the
selected initramfs, constructs the userspace catalog, and atomically publishes
the generated bundle back into that child box. No host path or environment
selector participates in this exchange.

A combined image that carries a kernel is accepted only when that kernel's
SHA-256 matches the boot image associated with the resolved `vmlinux`.
Currently the generated boot form must yield an executable newc initramfs;
recognized firmware containers that contain only a disk/rootfs remain
unsuitable for these fixed direct-kernel profiles.

Run the command from the Sarun TUI. When the provider is ready, the engine
opens its already-running GDB PTY in the TUI; F11 embeds that terminal in the
detail pane. The engine owns and tears down GDB, the facade, QEMU, the PTY, and
the one-use service streams with the box.

In the Changes pane, the corresponding pair workflow is: mark exactly one
kernel file, select the initramfs file, choose **Boot marked kernel + selected
initramfs…**, then choose the fixed machine architecture.

The selected kernel must match the boot-image SHA-256 in the kernel debugger
bundle visible from the captured build context; this is what prevents GDB from
loading a different `vmlinux`. A separately published image bundle is not
required: when no exact one exists, the same transactional provider path
derives it from the selected initramfs and finite provenance catalog.

## Install the provider box

Build ViroS's pinned tools and publish the provider once per host architecture:

```sh
./viros.sh download
./viros.sh build
./viros.sh sarun-install
```

`sarun-install` creates or updates `VIROS-DEBUG`. The box declares fixed
provider and GDB-client entry points through Sarun's authenticated typed
`service declare` request. It contains its own managed Python and GDB; GDB's
libpython lookup is relative to the provider box, and installation checks the
copied GDB before publishing the service.

## Publish an image from its build box

Build the kernel support and image bundle inside the same named Sarun box that
contains the SDK, Kbuild output, userspace build, and source tree. For example:

```sh
./viros.sh kernel-support ...
./viros.sh kernel-bundle \
  --arch aarch64 \
  --kbuild-output build_dir/.../linux-* \
  --output-dir artifacts/openwrt-kernel.bundle \
  ...

./viros.sh image-bundle \
  --arch aarch64 \
  --rootfs bin/targets/armsr/armv8/rootfs \
  --kernel-bundle artifacts/openwrt-kernel.bundle \
  --output-dir artifacts/openwrt-debug.image
```

`kernel-bundle` takes `vmlinux` and the architecture's standard boot image
directly from that exact Kbuild output. It reuses literal compiler, linker,
objcopy, and make commands recorded in Kbuild `.cmd` files when they are
unique and still available in the captured box. It does not search guessed SDK
locations. Kbuild often omits the top-level make identity, and LLVM names do
not determine `CROSS_COMPILE`; in those cases publication stops with the one
explicit build-time option that remains necessary.

`image-bundle` finds regular executable ELFs in the rootfs, then searches the
captured current build tree and the artifact parents for unstripped DWARF ELFs
with the same architecture and GNU build ID. Unique matches enter the catalog
automatically; unmatched programs remain valid rootfs content, while different
DWARF contents claiming one needed build ID are rejected. The rootfs, output,
and kernel bundle are excluded from discovery. An exceptional association can
still be stated as `--executable GUEST-PATH=UNSTRIPPED-ELF`; explicit entries
and discovered entries are combined without duplicate guest paths.

Publication records matching GNU build IDs, architecture, content hashes,
sizes, and DWARF line information. It creates a deterministic initramfs and
includes the exact kernel bundle.

The source tree must remain captured in that build box at the compilation
paths recorded in DWARF. The debugger client sees the build box as a read-only
Sarun attachment, so normal GDB source lookup resolves those paths without a
manual `directory` or `set substitute-path` step.

## Runtime behavior

The engine performs this sequence:

1. Resolve and validate the unique named provider and architecture-matching
   kernel bundle.
2. Re-read the selected boot artifact and its finite build ancestry from their
   owning boxes and verify their identities.
3. Reuse an exact image bundle when present; otherwise prepare and commit one
   in an ephemeral provider child, then validate every returned manifest and
   artifact again through the ordinary engine bundle validators.
4. Start the ViroS facade with the inherited QEMU RSP stream and attach the
   selected build context read-only.
5. Start GDB in an engine-owned PTY and pass it a typed, terminal-safe start
   record. The client revalidates each debug ELF's size, SHA-256, ELF
   class/machine, and GNU build ID.
6. Connect GDB through the engine-issued raw service. Only after the facade
   and managed client are ready does Sarun release the debug barrier.
7. Associate each observed Linux process inferior with its cataloged guest
   executable and exact debug ELF. Kernel symbols remain available as a
   separate GDB inferior.

For Sarun's ordinary x86-64 and AArch64 appliance mode, PID 1 waits before it
spawns the requested command. For a published firmware initramfs, the provider
owns the reset-stopped QEMU long enough to reach an exact first-userspace
boundary from the matching `vmlinux` (`start_thread`, `compat_start_thread`,
or `ret_to_user`). It then removes its bootstrap breakpoints, discovers the
initial tasks, and publishes the GDB service. GDB therefore opens with the
declared `rdinit` stopped at that boundary rather than with the CPU at reset.
The fixed boot profiles pass `nokaslr`, so the published kernel symbols remain
valid throughout this handoff.

## Fixed machine profiles

Integrated selected-boot profiles currently exist for:

| Sarun architecture | QEMU contract |
| --- | --- |
| `x86_64` | `microvm`, x86-64 initramfs |
| `aarch64` | `virt`, AArch64 initramfs |
| `arm` | `virt` with `cortex-a15`, ARMv7 initramfs |
| `mmips` | Malta with `24Kc`, little-endian MIPS32 initramfs |

The MMIPS profile matches OpenWrt Malta; it does not pretend that all
RouterBOARD models have Malta hardware. A RouterBOARD kernel that requires a
different board shape needs another explicit, fixed ViroS profile.

## Current boundary

Kernel and Linux processes are presented through GDB's multiprocess model.
Fatal userspace signals and kernel-die stops require the matching built-in
ViroS event support from the exact kernel bundle. New bundles fail construction
unless that event boundary and the architecture's first-userspace boundary are
present.

Automatic association currently covers the main executable of each process
whose rootfs build ID has one unique unstripped match in the captured build
tree. Shared-library symbol loading is not yet automatic, and programs without
a match remain visible as inferiors without source symbols. The integration
rejects missing or ambiguous resources instead of guessing a host installation
path.
