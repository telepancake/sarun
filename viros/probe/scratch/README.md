# Reserved AArch64 kernel scratch pages

This optional helper gives the reversible call gate three explicit pages in a
locally built, exact OpenWrt kernel.  It has no initialization function,
thread, syscall, module entry point, exported runtime API, or normal execution
path.  It only contributes storage and six stable symbols to `vmlinux`.

The code page is executable because the debugger temporarily places its
audited probe there.  In the linked kernel every aligned instruction slot is
normally an AArch64 `BRK #0x5653`, so an accidental branch stops instead of
executing zeroes or falling through.  The data and stack pages are distinct,
page-aligned BSS ranges and are non-executable under the kernel's normal
section permissions.

## Include it in an OpenWrt kernel build

Use the exact OpenWrt kernel tree and configuration that produce the guest's
`vmlinux`.  Copy this `kernel` directory to an otherwise unused built-in
directory, for example:

```text
linux-*/kernel/viros/Kbuild
linux-*/kernel/viros/viros_scratch.S
```

Then add this one line to that tree's `kernel/Makefile`:

```make
obj-y += viros/
```

Rebuild the kernel normally through OpenWrt.  The `obj-y` is intentional: this
must be present in the exact `vmlinux`, not built or loaded as a module.  Keep
the resulting unstripped `vmlinux`; its hash and GNU build ID bind subsequent
probe artifacts to the same build.

Discover the link-time guest virtual addresses and sizes with:

```sh
python3 probe/scratch/scratch_tool.py \
  path/to/exact/vmlinux --output build/my-openwrt/scratch.json
```

For a kernel booted with KASLR, determine its runtime relocation independently
and pass `--runtime-offset 0x...`; otherwise use the default zero offset (or
boot with KASLR disabled).  The tool validates symbol completeness, page
alignment, non-overlap, and final ELF section permissions.  It deliberately
does not guess physical addresses: obtain each GPA from the stopped guest via
QEMU's `gva2gpa` immediately before building the call-gate manifest.

Once the probe package has been linked at the reported code GVA, the scratch
document removes the six repeated GVA/size arguments from manifest creation:

```sh
python3 probe/probe_tool.py callgate-manifest \
  build/my-openwrt/probe-package/package.json \
  --vmlinux path/to/exact/vmlinux \
  --scratch-regions build/my-openwrt/scratch.json \
  --code-gpa 0x... --data-gpa 0x... --stack-gpa 0x... \
  --cpu 0 --init-task 0xffff... \
  --output build/my-openwrt/callgate.json
```

The bridge rechecks that the scratch document, probe package, and supplied
`vmlinux` have exactly the same SHA-256 and GNU build ID.  The original fully
explicit GVA/GPA/size mode remains available, but the two modes cannot be
mixed.
