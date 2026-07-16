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
qemu_sha=3745f6ea88e2e87fe0dc838b2b1d4e0a770bf48e01a1d5a186842a1fff76ccf5
linux_sha=ac26e508abd56e9f8b89872b6e10c49fc823bcc70d8068a5d8504c1a7c4ff045

mkdir -p "$sources" "$trees" "$build" "$out"

fetch() {
    local url=$1 file=$2 sha=$3
    if [[ ! -f $file ]]; then
        curl -fL --retry 3 -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    printf '%s  %s\n' "$sha" "$file" | sha256sum -c -
}

extract() {
    local archive=$1 directory=$2
    [[ -d $directory ]] || tar -C "$trees" -xf "$archive"
}

build_qemu() {
    fetch "https://download.qemu.org/qemu-$qemu_version.tar.xz" \
        "$sources/qemu-$qemu_version.tar.xz" "$qemu_sha"
    extract "$sources/qemu-$qemu_version.tar.xz" "$trees/qemu-$qemu_version"
    if ! grep -q "Sarun's appliance has no CXL" \
        "$trees/qemu-$qemu_version/hw/arm/virt-acpi-build.c"; then
        patch -d "$trees/qemu-$qemu_version" -p1 \
            < "$repo/engine/appliance/qemu-sarun.patch"
    fi
    local host_arch qbuild python
    host_arch=$(uname -m)
    qbuild=$build/qemu-$qemu_version-$host_arch-host-sarun
    python=$(uv python find 3.12 2>/dev/null || true)
    if [[ -z $python ]]; then
        uv python install 3.12
        python=$(uv python find 3.12)
    fi
    install -m644 "$repo/engine/appliance/qemu-aarch64.mak" \
        "$trees/qemu-$qemu_version/configs/devices/aarch64-softmmu/sarun.mak"
    install -m644 "$repo/engine/appliance/qemu-x86_64.mak" \
        "$trees/qemu-$qemu_version/configs/devices/x86_64-softmmu/sarun.mak"
    # QEMU's device Kconfig output survives reconfigure.  A fresh build tree is
    # required when the deliberately tiny device manifests change.
    rm -rf "$qbuild"
    mkdir -p "$qbuild"
    (cd "$qbuild" && "$trees/qemu-$qemu_version/configure" \
        --python="$python" \
        --target-list=aarch64-softmmu,x86_64-softmmu \
        --without-default-features --enable-system --enable-tcg \
        --enable-kvm --enable-vhost-user --enable-pie \
        --without-default-devices \
        --with-devices-aarch64=sarun --with-devices-x86_64=sarun)
    ninja -C "$qbuild" qemu-system-aarch64 qemu-system-x86_64
    mkdir -p "$out/host-$host_arch"
    install -m755 "$qbuild/qemu-system-aarch64" "$out/host-$host_arch/"
    install -m755 "$qbuild/qemu-system-x86_64" "$out/host-$host_arch/"
}

build_kernel() {
    fetch "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$linux_version.tar.xz" \
        "$sources/linux-$linux_version.tar.xz" "$linux_sha"
    extract "$sources/linux-$linux_version.tar.xz" "$trees/linux-$linux_version"
    for arch in aarch64 x86_64; do
        local karch target image kbuild
        case $arch in
            aarch64) karch=arm64; target=Image; image=arch/arm64/boot/Image ;;
            x86_64)  karch=x86_64; target=bzImage; image=arch/x86/boot/bzImage ;;
        esac
        kbuild=$build/linux-$linux_version-$arch
        make -C "$trees/linux-$linux_version" O="$kbuild" ARCH="$karch" LLVM=-21 tinyconfig
        "$trees/linux-$linux_version/scripts/kconfig/merge_config.sh" -m -O "$kbuild" \
            "$kbuild/.config" \
            "$repo/engine/appliance/kernel-common.config" \
            "$repo/engine/appliance/kernel-$arch.config"
        make -C "$trees/linux-$linux_version" O="$kbuild" ARCH="$karch" LLVM=-21 olddefconfig
        make -C "$trees/linux-$linux_version" O="$kbuild" ARCH="$karch" LLVM=-21 -j"$(nproc)" "$target"
        mkdir -p "$out/$arch"
        install -m644 "$kbuild/$image" "$out/$arch/kernel"
        install -m644 "$kbuild/.config" "$out/$arch/kernel.config"
    done
}

build_inner() {
    for arch in aarch64 x86_64; do
        local target
        target=$arch-unknown-linux-musl
        python3 "$repo/scripts/swipl.py" --target "$arch-linux-musl"
        rustup target add "$target"
        (cd "$repo/engine" && PATH="$(uv tool dir)/cargo-zigbuild/bin:$PATH" \
            cargo zigbuild --release --target "$target")
        mkdir -p "$out/$arch"
        install -m755 "$repo/engine/target/$target/release/sarun" "$out/$arch/init"
    done
}

case $mode in
    qemu) build_qemu ;;
    kernel) build_kernel ;;
    inner) build_inner ;;
    all) build_qemu; build_kernel; build_inner ;;
    *) echo "usage: $0 [qemu|kernel|inner|all]" >&2; exit 2 ;;
esac
