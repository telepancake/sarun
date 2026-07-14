#!/usr/bin/env bash
# Boot a tiny x86_64 Linux guest and run tv/sud32 against a freestanding i386
# program inside the guest. qemu-user is not sufficient for this check because
# sud requires PR_SET_SYSCALL_USER_DISPATCH; the real guest kernel provides it.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
sud32="${SUD32_BIN:-$repo_root/tv/sud32}"
kernel="${SUD32_QEMU_KERNEL:-}"

if [[ ! -x "$sud32" ]]; then
  echo "missing executable sud32 at $sud32 (build with: make -C tv sud32)" >&2
  exit 2
fi
if ! command -v qemu-system-x86_64 >/dev/null; then
  echo "qemu-system-x86_64 not found; install qemu-system-x86" >&2
  exit 2
fi
if ! command -v cpio >/dev/null; then
  echo "cpio not found" >&2
  exit 2
fi
if [[ -z "$kernel" ]]; then
  kernel="$(find /boot -maxdepth 1 -name 'vmlinuz-*' -type f | sort -V | tail -n1)"
fi
if [[ -z "$kernel" || ! -r "$kernel" ]]; then
  echo "no readable kernel found; set SUD32_QEMU_KERNEL=/path/to/bzImage" >&2
  exit 2
fi
busybox="${BUSYBOX:-$(command -v busybox || true)}"
if [[ -z "$busybox" || ! -x "$busybox" ]]; then
  echo "busybox not found; install busybox-static or set BUSYBOX=/path/to/busybox" >&2
  exit 2
fi

zig="${ZIG:-}"
if [[ -z "$zig" ]]; then
  if command -v uv >/dev/null; then
    zig="$(find "$(uv tool dir)/cargo-zigbuild" -path '*/ziglang/zig' -type f 2>/dev/null | head -n1 || true)"
  fi
fi
if [[ -z "$zig" || ! -x "$zig" ]]; then
  echo "zig not found; run: uv tool install --with ziglang cargo-zigbuild" >&2
  exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cat >"$work/exit32.s" <<'ASM'
.global _start
_start:
    mov $1, %eax
    xor %ebx, %ebx
    int $0x80
ASM
"$zig" cc -target x86-linux-musl -nostdlib -static -Wl,--build-id=none \
  -o "$work/exit32" "$work/exit32.s"

mkdir -p "$work/root"/{bin,dev,proc,tmp}
cp "$busybox" "$work/root/bin/busybox"
cp "$sud32" "$work/root/sud32"
cp "$work/exit32" "$work/root/exit32"
cat >"$work/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null || /bin/busybox mount -t tmpfs tmpfs /dev
/bin/busybox mkdir -p /dev/shm /tmp
/bin/busybox mount -t tmpfs tmpfs /dev/shm
/bin/busybox echo VM-START
/sud32 /exit32 >/tmp/sud32.out 2>/tmp/sud32.err
rc=$?
/bin/busybox echo SUD32-RC:$rc
/bin/busybox echo SUD32-ERR-BEGIN
/bin/busybox cat /tmp/sud32.err
/bin/busybox echo SUD32-ERR-END
/bin/busybox poweroff -f
INIT
chmod +x "$work/root/init"
(cd "$work/root" && find . -print0 | cpio --null -o --format=newc 2>/dev/null | gzip -9) >"$work/initramfs.cpio.gz"

log="$work/qemu.log"
timeout "${SUD32_QEMU_TIMEOUT:-60}" qemu-system-x86_64 \
  -M microvm -m "${SUD32_QEMU_MEM:-256M}" -nodefaults -no-reboot -nographic \
  -serial mon:stdio -kernel "$kernel" -initrd "$work/initramfs.cpio.gz" \
  -append 'console=ttyS0 panic=-1 quiet' >"$log" 2>&1 || {
    cat "$log" >&2
    exit 1
  }
cat "$log"
if ! grep -q 'SUD32-RC:0' "$log"; then
  echo "sud32 guest smoke failed" >&2
  exit 1
fi
