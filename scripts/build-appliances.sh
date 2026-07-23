#!/usr/bin/env bash
set -euo pipefail

# Reproducible external-cache builder for Sarun's deliberately narrow QEMU +
# Linux pairs.  Usage: scripts/build-appliances.sh [qemu|kernel|inner|all]

repo=$(cd "$(dirname "$0")/.." && pwd)
cache=${XDG_CACHE_HOME:-$HOME/.cache}/sarun
sources=$cache/appliance-sources
trees=$cache/appliances/src
build=$cache/appliances/build
out=$cache/appliances/v1
mode=${1:-all}
qemu_version=11.0.2
linux_version=6.18.38
slirp_revision=26be815b86e8d49add8c9a8b320239b9594ff03d
qemu_sha=3745f6ea88e2e87fe0dc838b2b1d4e0a770bf48e01a1d5a186842a1fff76ccf5
linux_sha=ac26e508abd56e9f8b89872b6e10c49fc823bcc70d8068a5d8504c1a7c4ff045
slirp_sha=abd190c8213f259ab9fc1a411b8dd87c54b7aeba329d3f87f4f4aa82d921bbc9

host_os=$(uname -s)
host_arch=$(uname -m)
[[ $host_arch == arm64 ]] && host_arch=aarch64
kernel_make=${SARUN_KERNEL_MAKE:-make}
kernel_llvm=${SARUN_KERNEL_LLVM:--21}
kernel_path=$PATH
kernel_host_args=()
case $host_os in
    Linux)
        host_output=host-$host_arch
        qemu_build_host=$host_arch-host
        qemu_accelerator_args=(--enable-kvm)
        qemu_pie_args=(--enable-pie)
        ;;
    Darwin)
        host_output=host-macos-$host_arch
        qemu_build_host=$host_arch-macos-host
        qemu_accelerator_args=(--enable-hvf)
        # QEMU's configure probe passes the ELF linker option `-pie`, which
        # Apple clang diagnoses as an unused argument under the probe's
        # `-Werror`. Mach-O executables are PIE by platform default; suppress
        # the inapplicable ELF probe instead of weakening compiler warnings.
        qemu_pie_args=(--disable-pie)
        # Homebrew keeps LLVM keg-only on macOS and may package lld
        # separately. Use their unversioned driver names (`LLVM=1`) without
        # changing Linux's established clang-21 selection. Both variables
        # remain overridable for builders with a pinned toolchain elsewhere.
        if [[ -z ${SARUN_KERNEL_LLVM:-} ]]; then
            llvm_prefix=$(brew --prefix llvm 2>/dev/null || true)
            gnu_sed_prefix=$(brew --prefix gnu-sed 2>/dev/null || true)
            coreutils_prefix=$(brew --prefix coreutils 2>/dev/null || true)
            lld_prefix=
            for lld_formula in lld lld@21 lld@20 lld@19; do
                candidate=$(brew --prefix "$lld_formula" 2>/dev/null || true)
                if [[ -x $candidate/bin/ld.lld ]]; then
                    lld_prefix=$candidate
                    break
                fi
            done
            if [[ ! -x $llvm_prefix/bin/clang || ! -x $lld_prefix/bin/ld.lld \
                    || ! -x $gnu_sed_prefix/libexec/gnubin/sed \
                    || ! -x $coreutils_prefix/libexec/gnubin/readlink ]]; then
                echo "Darwin appliance builds need Homebrew llvm, lld, gnu-sed, and coreutils" >&2
                exit 2
            fi
            kernel_llvm=1
            kernel_path=$gnu_sed_prefix/libexec/gnubin:$coreutils_prefix/libexec/gnubin:$llvm_prefix/bin:$lld_prefix/bin:$kernel_path
        fi
        # Kbuild also compiles small utilities that run on the build host.
        # Keep those on Apple clang (and its matching active macOS SDK) while
        # Homebrew clang/LLD produce the Linux target objects. macOS does not
        # ship <elf.h>; the already-pinned Zig distribution does. `-idirafter`
        # makes that one ABI header available without shadowing Apple SDK
        # headers such as stdint.h.
        zig_python=$(dirname "$(command -v python-zig)")/python
        zig_lib=$($zig_python -c \
            'import pathlib, ziglang; print(pathlib.Path(ziglang.__file__).parent / "lib")')
        zig_elf_header=$zig_lib/libc/musl/include/elf.h
        if [[ ! -f $zig_elf_header ]]; then
            echo "pinned Zig installation does not contain libc/musl/include/elf.h" >&2
            exit 2
        fi
        darwin_host_include=$build/darwin-host-include
        mkdir -p "$darwin_host_include/asm"
        install -m644 "$zig_elf_header" "$darwin_host_include/elf.h"
        install -m644 "$repo/engine/appliance/darwin-host-include/byteswap.h" \
            "$repo/engine/appliance/darwin-host-include/endian.h" \
            "$darwin_host_include/"
        install -m644 "$repo/engine/appliance/darwin-host-include/asm/types.h" \
            "$repo/engine/appliance/darwin-host-include/asm/posix_types.h" \
            "$darwin_host_include/asm/"
        kernel_host_args=(
            HOSTCC=/usr/bin/clang
            HOSTCXX=/usr/bin/clang++
            "HOSTCFLAGS=-idirafter $darwin_host_include"
        )
        if [[ -z ${SARUN_KERNEL_MAKE:-} ]] && command -v gmake >/dev/null 2>&1; then
            kernel_make=gmake
        fi
        ;;
    *)
        echo "unsupported appliance build host: $host_os" >&2
        exit 2
        ;;
esac

mkdir -p "$sources" "$trees" "$build" "$out"

verify_sha256() {
    local file=$1 expected=$2 actual
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file")
    else
        actual=$(shasum -a 256 "$file")
    fi
    actual=${actual%% *}
    if [[ $actual != "$expected" ]]; then
        echo "SHA-256 mismatch for $file: expected $expected, got $actual" >&2
        return 1
    fi
}

build_jobs() {
    if command -v nproc >/dev/null 2>&1; then
        nproc
    else
        sysctl -n hw.logicalcpu
    fi
}

fetch() {
    local url=$1 file=$2 sha=$3
    if [[ ! -f $file ]]; then
        curl -fL --retry 3 -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    verify_sha256 "$file" "$sha"
}

extract() {
    local archive=$1 directory=$2
    [[ -d $directory ]] || tar -C "$trees" -xf "$archive"
}

build_qemu() {
    fetch "https://download.qemu.org/qemu-$qemu_version.tar.xz" \
        "$sources/qemu-$qemu_version.tar.xz" "$qemu_sha"
    # QEMU's wrap is a Git clone from GitLab.  Fetch the same pinned revision
    # as a checked archive so the build has an explicit, mirrorable input and
    # does not perform an unverified network operation from Meson.
    fetch "https://codeload.github.com/qemu/libslirp/tar.gz/$slirp_revision" \
        "$sources/libslirp-$slirp_revision.tar.gz" "$slirp_sha"
    extract "$sources/qemu-$qemu_version.tar.xz" "$trees/qemu-$qemu_version"
    if [[ ! -d $trees/qemu-$qemu_version/subprojects/slirp ]]; then
        mkdir -p "$trees/qemu-$qemu_version/subprojects/slirp"
        tar -C "$trees/qemu-$qemu_version/subprojects/slirp" \
            --strip-components=1 -xf "$sources/libslirp-$slirp_revision.tar.gz"
    fi
    if ! grep -q "Sarun's appliance has no CXL" \
        "$trees/qemu-$qemu_version/hw/arm/virt-acpi-build.c"; then
        patch -d "$trees/qemu-$qemu_version" -p1 \
            < "$repo/engine/appliance/qemu-sarun.patch"
    fi
    local qbuild python
    qbuild=$build/qemu-$qemu_version-$qemu_build_host-sarun
    python=$(uv python find 3.12 2>/dev/null || true)
    if [[ -z $python ]]; then
        uv python install 3.12
        python=$(uv python find 3.12)
    fi
    install -m644 "$repo/engine/appliance/qemu-aarch64.mak" \
        "$trees/qemu-$qemu_version/configs/devices/aarch64-softmmu/sarun.mak"
    install -m644 "$repo/engine/appliance/qemu-x86_64.mak" \
        "$trees/qemu-$qemu_version/configs/devices/x86_64-softmmu/sarun.mak"
    install -m644 "$repo/engine/appliance/qemu-arm.mak" \
        "$trees/qemu-$qemu_version/configs/devices/arm-softmmu/sarun.mak"
    install -m644 "$repo/engine/appliance/qemu-mipsel.mak" \
        "$trees/qemu-$qemu_version/configs/devices/mipsel-softmmu/sarun.mak"
    # QEMU's device Kconfig output survives reconfigure.  A fresh build tree is
    # required when the deliberately tiny device manifests change.
    rm -rf "$qbuild"
    mkdir -p "$qbuild"
    (cd "$qbuild" && "$trees/qemu-$qemu_version/configure" \
        --python="$python" \
        --target-list=aarch64-softmmu,x86_64-softmmu,arm-softmmu,mipsel-softmmu \
        --without-default-features --enable-system --enable-tcg \
        "${qemu_accelerator_args[@]}" \
        --enable-vhost-user --enable-slirp "${qemu_pie_args[@]}" \
        --without-default-devices \
        --with-devices-aarch64=sarun --with-devices-x86_64=sarun \
        --with-devices-arm=sarun --with-devices-mipsel=sarun)
    ninja -C "$qbuild" \
        qemu-system-aarch64 qemu-system-x86_64 qemu-system-arm qemu-system-mipsel
    mkdir -p "$out/$host_output"
    install -m755 "$qbuild/qemu-system-aarch64" "$out/$host_output/"
    install -m755 "$qbuild/qemu-system-x86_64" "$out/$host_output/"
    install -m755 "$qbuild/qemu-system-arm" "$out/$host_output/"
    install -m755 "$qbuild/qemu-system-mipsel" "$out/$host_output/"
    if [[ $host_os == Darwin ]]; then
        # Meson's in-tree libslirp fallback is a dylib on Darwin. QEMU records
        # @loader_path/subprojects/slirp as its first rpath, so preserve that
        # relative layout and keep the cached appliance runnable without
        # mutating the binary's load commands.
        mkdir -p "$out/$host_output/subprojects/slirp"
        install -m755 "$qbuild/subprojects/slirp/libslirp.0.dylib" \
            "$out/$host_output/subprojects/slirp/"
    fi
    mkdir -p "$out/$host_output/LICENSES"
    install -m644 "$trees/qemu-$qemu_version/COPYING" \
        "$out/$host_output/LICENSES/QEMU-GPL-2.0.txt"
    install -m644 "$trees/qemu-$qemu_version/COPYING.LIB" \
        "$out/$host_output/LICENSES/QEMU-LGPL-2.1.txt"
    install -m644 "$trees/qemu-$qemu_version/LICENSE" \
        "$out/$host_output/LICENSES/QEMU-LICENSE.txt"
    install -m644 "$trees/qemu-$qemu_version/subprojects/slirp/COPYRIGHT" \
        "$out/$host_output/LICENSES/libslirp-COPYRIGHT.txt"
    mkdir -p "$out/$host_output/share/qemu"
    install -m644 "$trees/qemu-$qemu_version/pc-bios/bios-microvm.bin" \
        "$trees/qemu-$qemu_version/pc-bios/qboot.rom" \
        "$trees/qemu-$qemu_version/pc-bios/linuxboot_dma.bin" \
        "$out/$host_output/share/qemu/"
}

build_kernel() {
    fetch "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$linux_version.tar.xz" \
        "$sources/linux-$linux_version.tar.xz" "$linux_sha"
    extract "$sources/linux-$linux_version.tar.xz" "$trees/linux-$linux_version"
    if [[ $host_os == Darwin ]]; then
        if ! grep -q "macOS SDK owns uuid_t" \
            "$trees/linux-$linux_version/scripts/mod/file2alias.c"; then
            patch -d "$trees/linux-$linux_version" -p1 \
                < "$repo/engine/appliance/linux-darwin-file2alias.patch"
        fi
        if ! grep -q "Darwin host builds lack Linux libelf" \
            "$trees/linux-$linux_version/arch/x86/Kconfig"; then
            patch -d "$trees/linux-$linux_version" -p1 \
                < "$repo/engine/appliance/linux-darwin-x86.patch"
        fi
    fi
    for arch in aarch64 x86_64; do
        local karch target image kbuild
        case $arch in
            aarch64) karch=arm64; target=Image; image=arch/arm64/boot/Image ;;
            x86_64)  karch=x86_64; target=bzImage; image=arch/x86/boot/bzImage ;;
        esac
        kbuild=$build/linux-$linux_version-$arch
        PATH="$kernel_path" "$kernel_make" -C "$trees/linux-$linux_version" \
            O="$kbuild" ARCH="$karch" LLVM="$kernel_llvm" \
            "${kernel_host_args[@]}" tinyconfig
        PATH="$kernel_path" "$trees/linux-$linux_version/scripts/kconfig/merge_config.sh" \
            -m -O "$kbuild" \
            "$kbuild/.config" \
            "$repo/engine/appliance/kernel-common.config" \
            "$repo/engine/appliance/kernel-$arch.config"
        PATH="$kernel_path" "$kernel_make" -C "$trees/linux-$linux_version" \
            O="$kbuild" ARCH="$karch" LLVM="$kernel_llvm" \
            "${kernel_host_args[@]}" olddefconfig
        PATH="$kernel_path" "$kernel_make" -C "$trees/linux-$linux_version" \
            O="$kbuild" ARCH="$karch" LLVM="$kernel_llvm" \
            "${kernel_host_args[@]}" -j"$(build_jobs)" "$target"
        mkdir -p "$out/$arch"
        install -m644 "$kbuild/$image" "$out/$arch/kernel"
        install -m644 "$kbuild/.config" "$out/$arch/kernel.config"
        mkdir -p "$out/$arch/LICENSES"
        install -m644 "$trees/linux-$linux_version/COPYING" \
            "$out/$arch/LICENSES/Linux-COPYING.txt"
    done
}

build_inner() {
    for arch in aarch64 x86_64; do
        local target
        target=$arch-unknown-linux-musl
        PATH="$(uv tool dir)/cargo-zigbuild/bin:$PATH" \
            python3 "$repo/scripts/swipl.py" --target "$arch-linux-musl"
        rustup target add "$target"
        (cd "$repo/engine" && PATH="$(uv tool dir)/cargo-zigbuild/bin:$PATH" \
            cargo zigbuild --release --target "$target")
        mkdir -p "$out/$arch"
        install -m755 "$repo/engine/target/$target/release/sarun" "$out/$arch/init"
        python3 "$repo/scripts/release_licenses.py" --target "$target" \
            --output "$out/$arch/LICENSES"
    done
}

case $mode in
    qemu) build_qemu ;;
    kernel) build_kernel ;;
    inner) build_inner ;;
    all) build_qemu; build_kernel; build_inner ;;
    *) echo "usage: $0 [qemu|kernel|inner|all]" >&2; exit 2 ;;
esac
