# Cloud Codex setup for sarun and sud32

This repository expects Codex cloud environments to be able to build sarun, build
both sud wrappers, and execute 32-bit test binaries. Codex cloud setup scripts run
with internet access and are cached by the environment; the later agent phase is
offline unless the environment explicitly enables agent internet access. Put the
package/tool installation below in the environment **setup script**, not in an
agent prompt.

## Required Codex environment properties

Use a Linux x86-64 environment whose host kernel supports both:

- `CONFIG_SYSCALL_USER_DISPATCH=y` (`prctl(PR_SET_SYSCALL_USER_DISPATCH)` must
  succeed). This is mandatory for `sud32` and `sud64` runtime tracing.
- `CONFIG_IA32_EMULATION=y`, with container/seccomp policy allowing execution of
  i386 ELF binaries. Without this, native `./tv/sud32 ...` fails with
  `Exec format error`.

`qemu-i386` is useful for running freestanding 32-bit unit binaries, but it is
not enough to validate `sud32`: qemu-user currently rejects the syscall user
dispatch `prctl`, so `qemu-i386 ./tv/sud32 ...` stops before tracing starts.

## Setup script

Configure the Codex cloud environment with a setup script equivalent to:

```bash
set -euxo pipefail

apt-get update
apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  git \
  make \
  pkg-config \
  python3 \
  qemu-user \
  rustup

# sarun and tv/sud builds use cargo-zigbuild plus zig's bundled musl and Linux
# UAPI headers. This is what makes sud32 build without a host i386 sysroot.
uv tool install --with ziglang cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
```

If your base image does not include `uv`, install it before the `uv tool install`
line (for example with your organization's approved uv installer or package).

## Build commands to verify during setup or maintenance

From the repository root:

```bash
make engine
make -C tv sud64 sud32 \
  SUD_ADDINS='sud/trace sud/path_remap sud/cmd-rewrite sud/fake-exec sud/inramfs' \
  SUD_CFLAGS='-O2 -Wall -Wextra -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 -ffreestanding -fno-builtin -fno-stack-protector -fno-pie -fomit-frame-pointer -I. -DSUD_ADDIN_TRACE -DSUD_ADDIN_PATH_REMAP -DSUD_ADDIN_CMD_REWRITE -DSUD_ADDIN_FAKE_EXEC -DSUD_ADDIN_INRAMFS'
make -C tv build/inramfs_test64 build/inramfs_test32 \
  SUD_CFLAGS='-O2 -Wall -Wextra -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 -ffreestanding -fno-builtin -fno-stack-protector -fno-pie -fomit-frame-pointer -I. -DSUD_ADDIN_TRACE'
```

## Runtime checks

Use these checks to confirm the environment can execute the binaries it builds:

```bash
./tv/build/inramfs_test64
./tv/build/inramfs_test32
./tv/sud64 /bin/true >/tmp/sud64.out 2>/tmp/sud64.err
```

For `sud32`, compile a tiny i386 program and run it through the native wrapper:

```bash
cat >/tmp/exit32.s <<'ASM'
.global _start
_start:
    mov $1, %eax
    xor %ebx, %ebx
    int $0x80
ASM
ZIG="$(find "$(uv tool dir)/cargo-zigbuild" -path '*/ziglang/zig' -type f | head -n1)"
"$ZIG" cc -target x86-linux-musl -nostdlib -static -Wl,--build-id=none \
  -o /tmp/exit32 /tmp/exit32.s
./tv/sud32 /tmp/exit32 >/tmp/sud32.out 2>/tmp/sud32.err
```

A passing environment exits `0`. If this returns `Exec format error`, the kernel
or container does not allow i386 ELF execution. If stderr says
`prctl(PR_SET_SYSCALL_USER_DISPATCH): Invalid argument`, the kernel/runtime does
not provide syscall user dispatch to that process; using qemu-user is the common
cause.

## Agent internet access

The setup script can use the network to install dependencies. Keep agent-phase
internet access off unless a task explicitly needs live network access; if it is
enabled, restrict the allowlist to the package/documentation domains needed for
that task.
