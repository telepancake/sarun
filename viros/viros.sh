#!/usr/bin/env bash
set -Eeuo pipefail

# Everything created by this script stays below the directory from which it is
# invoked.  Override only when an explicit separate work area is desired.
WORKDIR=${VIROS_WORKDIR:-"$PWD"}
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DOWNLOADS="$WORKDIR/downloads"
SOURCES="$WORKDIR/sources"
BUILD="$WORKDIR/build"
TOOLS="$WORKDIR/tools"
ARTIFACTS="$WORKDIR/artifacts"
IMAGES="$WORKDIR/images"

QEMU_VERSION=${QEMU_VERSION:-11.0.2}
GDB_VERSION=${GDB_VERSION:-17.2}
TILE_QEMU_VERSION=${TILE_QEMU_VERSION:-5.2.0}
LINUX_VERSION=${LINUX_VERSION:-5.6.3}
ROUTEROS_VERSION=${ROUTEROS_VERSION:-latest}
MIKROTIK_GPL_COMMIT=${MIKROTIK_GPL_COMMIT:-c3e110db1d35886c96ee14e16fc5a06bcac59692}
PPC_TOOLCHAIN_NAME=${PPC_TOOLCHAIN_NAME:-powerpc-e500mc--glibc--bleeding-edge-2020.08-1}
PPC_TOOLCHAIN_SHA256=${PPC_TOOLCHAIN_SHA256:-8cab4fbb645be782a6eaeb7b6afd75fda4c0dc8ca9a4095b0be9b6eeb29a9759}
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '2')}
DISK_SIZE=${DISK_SIZE:-64M}
DEBUG_BOOT_TIMEOUT=${DEBUG_BOOT_TIMEOUT:-30}

ARCHES=(x86 arm arm64 mipsbe mmips smips ppc tile)
RUN_TARGETS=(x86 arm arm64 mipsbe mmips smips ppc-e500-smp ppc-e500 ppc-440 ppc-83xx tile)

say() { printf '==> %s\n' "$*"; }
die() { printf 'viros.sh: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required host command not found: $1"; }

usage() {
    cat <<'EOF'
Usage: ./viros.sh <subcommand> [argument] [-- QEMU options...]

Stages:
  download              Download pinned QEMU/GDB sources and current RouterOS
  build                 Build emulators, Python GDB, and debug kernel(s)
  kernel-debug <target> Build a MikroTik-configured vmlinux with GDB scripts
  extract [arch|all]    Extract Linux images/initramfs from RouterOS NPKs
  prepare [arch|all]    Extract and create per-target raw ext2 disk images
  run <target> [-- ...] Prepare and run one target; extra args go to QEMU
  gdb <target> [remote] Attach Python-enabled GDB to a paused QEMU
  debug <target>        Boot matching debug kernel, stop after init, open GDB

Information:
  list                  Print accepted run targets and their current status
  doctor                Check host prerequisites

Configuration is via QEMU_VERSION, GDB_VERSION, ROUTEROS_VERSION, JOBS,
DISK_SIZE, and VIROS_WORKDIR.  All output remains inside VIROS_WORKDIR.
EOF
}

download_file() {
    local url=$1 destination=$2 partial="${2}.part"
    mkdir -p "$(dirname -- "$destination")"
    if [[ -s "$destination" ]]; then
        say "Already downloaded: $(basename -- "$destination")"
        return
    fi
    say "Downloading $url"
    curl --fail --location --retry 3 --continue-at - --output "$partial" "$url"
    mv -- "$partial" "$destination"
}

resolve_routeros_version() {
    if [[ "$ROUTEROS_VERSION" == latest ]]; then
        local latest
        latest=$(curl --fail --location --retry 3 --silent https://upgrade.mikrotik.com/routeros/LATEST.7)
        ROUTEROS_VERSION=${latest%%[[:space:]]*}
        [[ "$ROUTEROS_VERSION" =~ ^7\.[0-9]+([.][0-9]+)?$ ]] || die "unexpected RouterOS version response: $latest"
    fi
    printf '%s\n' "$ROUTEROS_VERSION" > "$DOWNLOADS/routeros-version"
    say "RouterOS version: $ROUTEROS_VERSION"
}

download_stage() {
    need curl
    mkdir -p "$DOWNLOADS"
    download_file "https://download.qemu.org/qemu-${QEMU_VERSION}.tar.xz" "$DOWNLOADS/qemu-${QEMU_VERSION}.tar.xz"
    download_file "https://ftp.gnu.org/gnu/gdb/gdb-${GDB_VERSION}.tar.xz" "$DOWNLOADS/gdb-${GDB_VERSION}.tar.xz"
    download_file "https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-${LINUX_VERSION}.tar.xz" "$DOWNLOADS/linux-${LINUX_VERSION}.tar.xz"
    # Last upstream release containing TILE-Gx translation.  It is built for
    # linux-user analysis; it is not presented as a full-system emulator.
    download_file "https://download.qemu.org/qemu-${TILE_QEMU_VERSION}.tar.xz" "$DOWNLOADS/qemu-${TILE_QEMU_VERSION}-tile-legacy.tar.xz"
    download_file "https://github.com/tikoci/mikrotik-gpl/archive/${MIKROTIK_GPL_COMMIT}.tar.gz" "$DOWNLOADS/mikrotik-gpl-${MIKROTIK_GPL_COMMIT}.tar.gz"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/powerpc-e500mc/tarballs/${PPC_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${PPC_TOOLCHAIN_NAME}.tar.bz2"
    resolve_routeros_version
    local arch suffix name
    for arch in "${ARCHES[@]}"; do
        if [[ "$arch" == x86 ]]; then
            suffix=""
        else
            suffix="-${arch}"
        fi
        name="routeros-${ROUTEROS_VERSION}${suffix}.npk"
        download_file "https://download.mikrotik.com/routeros/${ROUTEROS_VERSION}/${name}" "$DOWNLOADS/$name"
    done
    download_file "https://download.mikrotik.com/routeros/${ROUTEROS_VERSION}/chr-${ROUTEROS_VERSION}.img.zip" "$DOWNLOADS/chr-${ROUTEROS_VERSION}.img.zip"
}

unpack_source() {
    local archive=$1 directory=$2
    if [[ ! -f "$directory/.unpacked" ]]; then
        mkdir -p "$directory"
        tar -xf "$archive" -C "$directory" --strip-components=1
        : > "$directory/.unpacked"
    fi
}

build_qemu() {
    local src="$SOURCES/qemu-${QEMU_VERSION}" out="$BUILD/qemu-${QEMU_VERSION}"
    unpack_source "$DOWNLOADS/qemu-${QEMU_VERSION}.tar.xz" "$src"
    if [[ ! -f "$src/.routeros-arm-load" ]]; then
        grep -q '#define KERNEL_LOAD_ADDR 0x00010000' "$src/hw/arm/boot.c" || die "QEMU ARM raw load constant changed; cannot apply RouterOS TEXT_OFFSET fix"
        sed -i 's/#define KERNEL_LOAD_ADDR 0x00010000/#define KERNEL_LOAD_ADDR 0x00048000/' "$src/hw/arm/boot.c"
        : > "$src/.routeros-arm-load"
    fi
    if [[ ! -f "$src/.routeros-mips-vm" ]]; then
        say "Applying the RouterOS MetaROUTER/SMIPS Malta boot patch"
        patch --batch --forward -d "$src" -p1 < "$SCRIPT_DIR/qemu-mips-routeros.patch" ||
            die "RouterOS MIPS QEMU patch failed"
        : > "$src/.routeros-mips-vm"
    fi
    mkdir -p "$out" "$TOOLS/qemu"
    say "Configuring QEMU $QEMU_VERSION"
    (cd "$out" && "$src/configure" \
        --prefix="$TOOLS/qemu" \
        --target-list=x86_64-softmmu,x86_64-linux-user,arm-softmmu,aarch64-softmmu,mips-softmmu,mipsel-softmmu,ppc-softmmu,ppc64-softmmu \
        --disable-docs --disable-gtk --disable-sdl --disable-vnc \
        --disable-curl --disable-libssh --disable-rbd --disable-glusterfs)
    say "Building QEMU"
    make -C "$out" -j "$JOBS"
    make -C "$out" install
}

build_gdb() {
    local src="$SOURCES/gdb-${GDB_VERSION}" out="$BUILD/gdb-${GDB_VERSION}"
    unpack_source "$DOWNLOADS/gdb-${GDB_VERSION}.tar.xz" "$src"
    mkdir -p "$out" "$TOOLS/gdb"
    local python
    python=$(command -v python3)
    say "Configuring Python-enabled multi-target GDB $GDB_VERSION"
    (cd "$out" && "$src/configure" \
        --prefix="$TOOLS/gdb" --enable-targets=all --with-python="$python" \
        --disable-binutils --disable-gas --disable-gold --disable-gprof \
        --disable-ld --disable-sim)
    say "Building GDB"
    make -C "$out" -j "$JOBS" all-gdb
    make -C "$out" install-gdb
}

verify_file() {
    local expected=$1 file=$2 actual
    actual=$(sha256sum "$file" | awk '{print $1}')
    [[ "$actual" == "$expected" ]] || die "checksum mismatch for $file: expected $expected, got $actual"
}

unpack_ppc_toolchain() {
    local archive="$DOWNLOADS/${PPC_TOOLCHAIN_NAME}.tar.bz2" destination="$TOOLS/cross-powerpc"
    [[ -s "$archive" ]] || die "PowerPC toolchain is missing; run download first"
    verify_file "$PPC_TOOLCHAIN_SHA256" "$archive"
    if [[ ! -f "$destination/.unpacked" ]]; then
        mkdir -p "$destination"
        tar -xf "$archive" -C "$destination" --strip-components=1
        : > "$destination/.unpacked"
    fi
}

mikrotik_kernel_source() {
    printf '%s\n' "$SOURCES/linux-${LINUX_VERSION}-mikrotik"
}

prepare_mikrotik_source() {
    local archive="$DOWNLOADS/mikrotik-gpl-${MIKROTIK_GPL_COMMIT}.tar.gz"
    [[ -s "$archive" ]] || die "MikroTik GPL source archive is missing; run download first"
    unpack_source "$archive" "$SOURCES/mikrotik-gpl"
    [[ -s "$SOURCES/mikrotik-gpl/2025-03-19/linux-${LINUX_VERSION}.patch" ]] || die "MikroTik Linux patch is absent from the GPL archive"
}

prepare_mikrotik_kernel_source() {
    local src patchfile archive
    prepare_mikrotik_source
    src=$(mikrotik_kernel_source)
    patchfile="$SOURCES/mikrotik-gpl/2025-03-19/linux-${LINUX_VERSION}.patch"
    archive="$DOWNLOADS/linux-${LINUX_VERSION}.tar.xz"
    [[ -s "$archive" ]] || die "official Linux $LINUX_VERSION source is missing; run download first"
    if [[ ! -f "$src/.mikrotik-patched" ]]; then
        if [[ -e "$src" ]]; then
            die "$src exists without a completed patch marker; move it aside and retry"
        fi
        mkdir -p "$src"
        tar -xf "$archive" -C "$src" --strip-components=1
        say "Applying MikroTik's disclosed Linux $LINUX_VERSION patch"
        if ! patch --batch --forward -d "$src" -p1 < "$patchfile"; then
            die "MikroTik kernel patch failed; remove the incomplete $src before retrying"
        fi
        : > "$src/.mikrotik-patched"
    fi
    [[ -f "$src/arch/arm/kernel/head.S" && -f "$src/arch/powerpc/kernel/head_44x.S" ]] ||
        die "patched Linux source is incomplete"
}

PPC_CROSS_PREFIX=
setup_ppc_cross() {
    local real_bin compiler emulator_root wrapper_bin tool compiler_version
    unpack_ppc_toolchain
    compiler=$(find "$TOOLS/cross-powerpc" -path '*/bin/powerpc-linux-gcc' -print -quit)
    [[ -n "$compiler" ]] || die "powerpc-linux-gcc was not found in the Bootlin toolchain"
    real_bin=$(dirname -- "$compiler")
    if [[ $(uname -m) == x86_64 ]]; then
        PPC_CROSS_PREFIX="$real_bin/powerpc-linux-"
    else
        [[ -x "$TOOLS/qemu/bin/qemu-x86_64" ]] || die "qemu-x86_64 is required to run the pinned PowerPC compiler on $(uname -m); run build first"
        emulator_root="$TOOLS/cross-powerpc-emulated/root"
        wrapper_bin="$TOOLS/cross-powerpc-emulated/bin"
        mkdir -p "$emulator_root/lib64" "$wrapper_bin"
        [[ -s "$TOOLS/cross-powerpc/lib/ld-linux-x86-64.so.2" ]] || die "Bootlin x86-64 loader was not found"
        cp -f -- "$TOOLS/cross-powerpc/lib/ld-linux-x86-64.so.2" "$emulator_root/lib64/ld-linux-x86-64.so.2"
        chmod +x "$SCRIPT_DIR/emulated-cross-tool"
        for tool in gcc ld as nm objcopy objdump strip ar ranlib readelf size strings; do
            ln -sfn "$SCRIPT_DIR/emulated-cross-tool" "$wrapper_bin/powerpc-linux-$tool"
        done
        export VIROS_QEMU_X86_64="$TOOLS/qemu/bin/qemu-x86_64"
        export VIROS_X86_LD_ROOT="$emulator_root"
        export VIROS_CROSS_REAL_BIN="$real_bin"
        PPC_CROSS_PREFIX="$wrapper_bin/powerpc-linux-"
    fi
    compiler_version=$("$PPC_CROSS_PREFIX"gcc --version)
    [[ "${compiler_version%%$'\n'*}" == *10.2.0* ]] || die "the pinned PowerPC compiler is not GCC 10.2.0"
}

find_kernel_config() {
    local candidate
    for candidate in "$@"; do
        if [[ -s "$SOURCES/mikrotik-gpl/2025-03-19/configs/$candidate" ]]; then
            printf '%s\n' "$SOURCES/mikrotik-gpl/2025-03-19/configs/$candidate"
            return
        fi
    done
    die "none of the expected MikroTik configs were found: $*"
}

build_debug_kernel() {
    local target=${1:-} src out config arch prefix= cc= image= obj raw compiler_version
    local -a targets
    case "$target" in
        arm|arm64|smips|ppc-e500-smp|ppc-440) ;;
        *) die "no validated matching debug-kernel boot for $target" ;;
    esac
    need flex; need bison; need bc; need perl
    prepare_mikrotik_kernel_source
    src=$(mikrotik_kernel_source)
    out="$BUILD/kernel-$target"
    case "$target" in
        arm)
            arch=arm
            config=$(find_kernel_config arm.config)
            if command -v arm-linux-gnueabi-gcc-11 >/dev/null 2>&1; then
                prefix=arm-linux-gnueabi-; cc=arm-linux-gnueabi-gcc-11
            elif command -v arm-linux-gnueabi-gcc >/dev/null 2>&1; then
                prefix=arm-linux-gnueabi-
            else
                die "an ARM EABI cross compiler is required (arm-linux-gnueabi-gcc)"
            fi
            image=zImage
            ;;
        arm64)
            arch=arm64
            config=$(find_kernel_config arm64.config aarch64.config)
            if [[ $(uname -m) == aarch64 ]]; then
                prefix=
            elif command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
                prefix=aarch64-linux-gnu-
            else
                die "an AArch64 cross compiler is required (aarch64-linux-gnu-gcc)"
            fi
            image=Image
            ;;
        smips)
            arch=mips
            config=$(find_kernel_config smips.config)
            command -v mips-linux-gnu-gcc >/dev/null 2>&1 ||
                die "a big-endian MIPS cross compiler is required (mips-linux-gnu-gcc)"
            prefix=mips-linux-gnu-
            ;;
        ppc-e500-smp)
            arch=powerpc; config=$(find_kernel_config e500-smp.config)
            setup_ppc_cross; prefix=$PPC_CROSS_PREFIX
            ;;
        ppc-440)
            arch=powerpc; config=$(find_kernel_config 440.config)
            setup_ppc_cross; prefix=$PPC_CROSS_PREFIX
            ;;
    esac
    mkdir -p "$out" "$ARTIFACTS/$target"
    cp -f -- "$config" "$out/.config"
    "$src/scripts/config" --file "$out/.config" \
        --enable GDB_SCRIPTS --enable DEBUG_INFO \
        --disable DEBUG_INFO_REDUCED --disable DEBUG_INFO_SPLIT --disable DEBUG_INFO_DWARF4
    if [[ "$target" == smips ]]; then
        "$src/scripts/config" --file "$out/.config" \
            --enable DEBUG_INFO_DWARF4 --enable KALLSYMS --enable KALLSYMS_ALL --enable IKCONFIG
    fi
    local make_args=( -C "$src" O="$out" ARCH="$arch" CROSS_COMPILE="$prefix" )
    [[ -n "$cc" ]] && make_args+=( CC="$cc" )
    say "Building patched Linux $LINUX_VERSION for $target with MikroTik's published config"
    make "${make_args[@]}" olddefconfig
    targets=(vmlinux scripts_gdb)
    [[ -n "$image" ]] && targets+=("$image")
    make "${make_args[@]}" -j "$JOBS" "${targets[@]}"
    cp -f -- "$out/vmlinux" "$ARTIFACTS/$target/vmlinux.debug"
    [[ -s "$out/vmlinux-gdb.py" ]] || die "kernel build did not create vmlinux-gdb.py"
    case "$target" in
        arm)
            cp -f -- "$out/arch/arm/boot/zImage" "$ARTIFACTS/arm/kernel.debug.zImage"
            ;;
        arm64)
            cp -f -- "$out/arch/arm64/boot/Image" "$ARTIFACTS/arm64/kernel.debug.Image"
            ;;
        smips)
            cp -f -- "$out/vmlinux" "$ARTIFACTS/smips/kernel.debug.qemu.elf"
            ;;
        ppc-*)
            raw="$ARTIFACTS/$target/kernel.debug.raw"
            obj="$out/kernel.debug.raw.o"
            "$prefix"objcopy -O binary "$out/vmlinux" "$raw"
            "$prefix"objcopy -I binary -O elf32-powerpc -B powerpc "$raw" "$obj"
            "$prefix"objcopy --set-section-flags .data=alloc,load,code,data,contents "$obj"
            "$prefix"ld -Ttext=0 -e 0 -o "$ARTIFACTS/$target/kernel.debug.qemu.elf" "$obj"
            ;;
    esac
    {
        printf 'gpl_commit  %s\n' "$MIKROTIK_GPL_COMMIT"
        sha256sum "$DOWNLOADS/linux-${LINUX_VERSION}.tar.xz" \
            "$SOURCES/mikrotik-gpl/2025-03-19/linux-${LINUX_VERSION}.patch" "$config"
        if [[ -n "$cc" ]]; then
            compiler_version=$("$cc" --version)
        elif [[ -n "$prefix" ]]; then
            compiler_version=$("$prefix"gcc --version)
        else
            compiler_version=$(gcc --version)
        fi
        printf '%s\n' "${compiler_version%%$'\n'*}"
    } > "$ARTIFACTS/$target/debug-build.provenance"
    say "Matching debug vmlinux: $ARTIFACTS/$target/vmlinux.debug"
}

build_tile_linux_user() {
    local src="$SOURCES/qemu-${TILE_QEMU_VERSION}-tile-legacy" out="$BUILD/qemu-${TILE_QEMU_VERSION}-tile-legacy"
    unpack_source "$DOWNLOADS/qemu-${TILE_QEMU_VERSION}-tile-legacy.tar.xz" "$src"
    mkdir -p "$out" "$TOOLS/tile-legacy"
    say "Configuring legacy TILE-Gx linux-user translator"
    (cd "$out" && "$src/configure" --prefix="$TOOLS/tile-legacy" \
        --target-list=tilegx-linux-user --disable-system --disable-docs \
        --disable-gtk --disable-sdl --disable-vnc)
    make -C "$out" -j "$JOBS"
    make -C "$out" install
}

build_mikrotik_tile_kvm() {
    local base patchfile src out
    prepare_mikrotik_source
    base="$SOURCES/mikrotik-gpl/2025-03-19/qemu-2.0.2"
    patchfile="$SOURCES/mikrotik-gpl/2025-03-19/qemu-2.0.2.patch"
    src="$SOURCES/qemu-2.0.2-mikrotik-tile"
    out="$BUILD/qemu-2.0.2-mikrotik-tile"
    [[ -f "$base/configure" && -s "$patchfile" ]] || die "MikroTik's disclosed QEMU 2.0.2 tree/patch is missing"
    if [[ ! -f "$src/.patched" ]]; then
        mkdir -p "$src"
        cp -a "$base/." "$src/"
        patch -d "$src" -p1 < "$patchfile"
        : > "$src/.patched"
    fi
    if [[ $(uname -m) != tilegx ]]; then
        say "Prepared MikroTik's TILE softmmu source (build skipped: it is native TILE-Gx KVM-only, host is $(uname -m))"
        return
    fi
    mkdir -p "$out" "$TOOLS/tile-kvm"
    say "Building MikroTik's TILE-Gx KVM-only QEMU on a native TILE-Gx host"
    (cd "$out" && "$src/configure" --prefix="$TOOLS/tile-kvm" \
        --target-list=tilegx-softmmu --enable-kvm --disable-docs \
        --disable-gtk --disable-sdl --disable-vnc)
    make -C "$out" -j "$JOBS"
    make -C "$out" install
}

build_stage() {
    need make; need tar; need python3; need gcc; need g++; need pkg-config; need sha256sum
    [[ -s "$DOWNLOADS/qemu-${QEMU_VERSION}.tar.xz" ]] || die "run download first"
    build_qemu
    build_gdb
    build_tile_linux_user
    build_mikrotik_tile_kvm
    build_debug_kernel arm
    build_debug_kernel arm64
    build_debug_kernel smips
    build_debug_kernel ppc-e500-smp
    build_debug_kernel ppc-440
}

routeros_version() {
    if [[ -s "$DOWNLOADS/routeros-version" ]]; then
        head -n 1 "$DOWNLOADS/routeros-version"
    elif [[ "$ROUTEROS_VERSION" != latest ]]; then
        printf '%s\n' "$ROUTEROS_VERSION"
    else
        local found
        found=$(find "$WORKDIR" -maxdepth 1 -type f -name 'routeros-*.npk' -printf '%f\n' 2>/dev/null | sed -n 's/^routeros-\([0-9][0-9.]*\)\(-[^.]*\)\?\.npk$/\1/p' | sort -V | tail -n 1)
        [[ -n "$found" ]] || die "cannot determine RouterOS version; run download or set ROUTEROS_VERSION"
        printf '%s\n' "$found"
    fi
}

npk_for_arch() {
    local arch=$1 version suffix candidate
    version=$(routeros_version)
    [[ "$arch" == x86 ]] && suffix="" || suffix="-${arch}"
    for candidate in "$DOWNLOADS/routeros-${version}${suffix}.npk" "$WORKDIR/routeros-${version}${suffix}.npk"; do
        if [[ -s "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return
        fi
    done
    die "RouterOS NPK for $arch not found; run download"
}

extract_one() {
    local arch=$1 npk
    npk=$(npk_for_arch "$arch")
    say "Extracting $arch from $(basename -- "$npk")"
    python3 "$SCRIPT_DIR/npk_extract.py" "$npk" "$arch" "$ARTIFACTS"
}

extract_stage() {
    need python3
    local wanted=${1:-all} arch
    if [[ "$wanted" == all ]]; then
        for arch in "${ARCHES[@]}"; do extract_one "$arch"; done
    else
        [[ " ${ARCHES[*]} " == *" $wanted "* ]] || die "unknown RouterOS architecture: $wanted"
        extract_one "$wanted"
    fi
}

base_arch() {
    case "$1" in
        ppc-*) printf 'ppc\n' ;;
        *) printf '%s\n' "$1" ;;
    esac
}

create_disk() {
    local target=$1 arch npk stage disk
    arch=$(base_arch "$target")
    npk=$(npk_for_arch "$arch")
    stage="$BUILD/disk-root-$target"
    disk="$IMAGES/$target.raw"
    mkdir -p "$stage" "$IMAGES"
    cp -f -- "$npk" "$stage/routeros.npk"
    printf '%s\n' "$target" > "$stage/VIROS_TARGET"
    truncate -s "$DISK_SIZE" "$disk"
    # -d populates the filesystem without loop devices or root privileges.
    mkfs.ext2 -q -F -L ROUTEROS -d "$stage" "$disk"
    say "Created $disk ($DISK_SIZE ext2, containing routeros.npk)"
}

prepare_one() {
    local target=$1 arch
    arch=$(base_arch "$target")
    extract_one "$arch"
    create_disk "$target"
    if [[ "$target" == x86 ]]; then
        local version zip
        version=$(routeros_version)
        zip="$DOWNLOADS/chr-${version}.img.zip"
        if [[ -s "$zip" ]]; then
            need unzip
            unzip -p "$zip" > "$IMAGES/chr-${version}.img"
        fi
    fi
}

prepare_ppc440_dtb() {
    local source= candidate out="$ARTIFACTS/ppc-440/qemu-bios" dts="$ARTIFACTS/ppc-440/canyonlands-rb1200.dts"
    need dtc; need sed
    for candidate in \
        "$SOURCES/qemu-${QEMU_VERSION}/pc-bios/dtb/canyonlands.dtb" \
        "$TOOLS/qemu/share/qemu/dtb/canyonlands.dtb" \
        "$TOOLS/qemu/share/qemu/canyonlands.dtb" \
        /usr/share/qemu/canyonlands.dtb; do
        if [[ -s "$candidate" ]]; then source=$candidate; break; fi
    done
    [[ -n "$source" ]] || die "QEMU's base canyonlands.dtb was not found; run download and build"
    mkdir -p "$out/dtb"
    dtc -q -I dtb -O dts -o "$dts" "$source"
    sed -i '0,/compatible = "amcc,canyonlands";/s//compatible = "RB1200", "amcc,canyonlands";/' "$dts"
    grep -q 'compatible = "RB1200", "amcc,canyonlands";' "$dts" || die "failed to add the RB1200 compatibility string to canyonlands.dtb"
    dtc -q -I dts -O dtb -o "$out/dtb/canyonlands.dtb" "$dts"
    printf '%s\n' "$out"
}

prepare_stage() {
    need truncate; need mkfs.ext2
    local wanted=${1:-all} target
    if [[ "$wanted" == all ]]; then
        for target in "${RUN_TARGETS[@]}"; do prepare_one "$target"; done
    else
        [[ " ${RUN_TARGETS[*]} " == *" $wanted "* ]] || die "unknown run target: $wanted"
        prepare_one "$wanted"
    fi
}

qemu_binary() {
    local name=$1
    if [[ -x "$TOOLS/qemu/bin/$name" ]]; then
        printf '%s\n' "$TOOLS/qemu/bin/$name"
    elif command -v "$name" >/dev/null 2>&1; then
        command -v "$name"
    else
        die "$name is not built; run download and build"
    fi
}

run_stage() {
    local target=${1:-}
    [[ -n "$target" ]] || die "run requires a target (see: ./viros.sh list)"
    shift || true
    [[ ${1:-} == -- ]] && shift
    [[ " ${RUN_TARGETS[*]} " == *" $target "* ]] || die "unknown run target: $target"

    case "$target" in
        tile)
            die "TILE cannot satisfy this host/trace criterion: MikroTik's disclosed tilegx-softmmu is native TILE-Gx KVM-only and its GDB register/memory hooks are unfinished; there is no TCG system emulation"
            ;;
        ppc-83xx)
            die "ppc-83xx cannot boot yet: QEMU has no MPC83xx machine matching the RB333/RB600 kernel"
            ;;
        ppc-e500)
            die "ppc-e500 is not a success: unlike e500-smp, this kernel lacks QEMU e500 platform support and does not reach init on ppce500"
            ;;
    esac

    prepare_one "$target"
    local qemu kernel initrd disk
    disk="$IMAGES/$target.raw"
    case "$target" in
        x86)
            qemu=$(qemu_binary qemu-system-x86_64)
            local version chr
            version=$(routeros_version); chr="$IMAGES/chr-${version}.img"
            [[ -s "$chr" ]] || die "CHR image missing; run download then prepare x86"
            exec "$qemu" -machine q35,accel=tcg -m 256M -nographic -no-reboot \
                -drive "file=$chr,format=raw,if=virtio" -nic user,model=virtio-net-pci "$@"
            ;;
        arm)
            qemu=$(qemu_binary qemu-system-arm); kernel="$ARTIFACTS/arm/kernel.raw"; initrd="$ARTIFACTS/arm/initramfs.cpio"
            exec "$qemu" -M virt -cpu cortex-a15 -smp 1 -m 512M \
                -display none -monitor none -serial none -no-reboot \
                -d in_asm -D "$ARTIFACTS/arm/qemu-in_asm.log" \
                -kernel "$kernel" -initrd "$initrd" \
                -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1' \
                -nic none "$@"
            ;;
        arm64)
            qemu=$(qemu_binary qemu-system-aarch64); kernel="$ARTIFACTS/arm64/kernel.raw"; initrd="$ARTIFACTS/arm64/initramfs.cpio"
            exec "$qemu" -M virt -cpu cortex-a57 -smp 2 -m 512M -nographic -no-reboot \
                -kernel "$kernel" -initrd "$initrd" \
                -append 'console=ttyAMA0,115200 earlycon=pl011,0x09000000 loglevel=8 ignore_loglevel init=/init panic=-1' \
                -nic none "$@"
            ;;
        mipsbe)
            qemu=$(qemu_binary qemu-system-mips); kernel="$ARTIFACTS/mipsbe/kernel.qemu.elf"; initrd="$ARTIFACTS/mipsbe/initramfs.cpio"
            exec "$qemu" -M malta -cpu 24Kc -m 256M -nographic -no-reboot -nodefaults -serial stdio -monitor none \
                -kernel "$kernel" -initrd "$initrd" -append 'console=ttyS0 root=/dev/ram0' \
                -nic none "$@"
            ;;
        smips)
            qemu="$TOOLS/qemu/bin/qemu-system-mips"; kernel="$ARTIFACTS/smips/kernel.qemu.elf"; initrd="$ARTIFACTS/smips/initramfs.cpio"
            [[ -x "$qemu" ]] || die "SMIPS requires viros's MetaROUTER-patched QEMU; run download and build"
            exec "$qemu" -M malta -cpu 24Kc -m 256M -display none -monitor none -serial none \
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd" \
                -append 'board=vm mem=256M HZ=100000000 init=/init panic=-1' \
                -nic none "$@"
            ;;
        mmips)
            qemu=$(qemu_binary qemu-system-mipsel); kernel="$ARTIFACTS/mmips/kernel.qemu.elf"; initrd="$ARTIFACTS/mmips/initramfs.cpio"
            exec "$qemu" -M malta -cpu 34Kf -m 256M -nographic -no-reboot -nodefaults -serial stdio -monitor none \
                -kernel "$kernel" -initrd "$initrd" -append 'console=ttyS0 root=/dev/ram0' \
                -nic none "$@"
            ;;
        ppc-e500-smp)
            qemu=$(qemu_binary qemu-system-ppc); kernel="$ARTIFACTS/$target/kernel.qemu.elf"; initrd="$ARTIFACTS/$target/initramfs.cpio"
            # The SMP-flavoured RouterOS image is the only one compiled with
            # QEMU e500 platform support, but its secondary-core bring-up
            # stalls on current QEMU.  One vCPU reaches /init reliably.
            exec "$qemu" -M ppce500 -cpu e500v2 -smp 1 -m 256M -nographic -no-reboot \
                -nodefaults -serial stdio -monitor none \
                -kernel "$kernel" -initrd "$initrd" -append 'console=ttyS0 root=/dev/ram0' \
                -nic none "$@"
            ;;
        ppc-440)
            local bios
            qemu=$(qemu_binary qemu-system-ppc); kernel="$ARTIFACTS/ppc-440/kernel.qemu.elf"; initrd="$ARTIFACTS/ppc-440/initramfs.cpio"
            bios=$(prepare_ppc440_dtb)
            exec "$qemu" -L "$bios" -M sam460ex -cpu 460exb -m 256M -nographic -no-reboot \
                -nodefaults -serial stdio -monitor none \
                -kernel "$kernel" -initrd "$initrd" -append 'console=ttyS0 root=/dev/ram0' \
                -nic none "$@"
            ;;
    esac
}

gdb_stage() {
    local target=${1:-} remote=${2:-:1234} gdb out vmlinux helper
    [[ -n "$target" ]] || die "gdb requires a target"
    case "$target" in
        arm|arm64|smips|ppc-e500-smp|ppc-440) ;;
        *) die "no validated matching debug kernel for $target" ;;
    esac
    gdb="$TOOLS/gdb/bin/gdb"
    out="$BUILD/kernel-$target"
    vmlinux="$ARTIFACTS/$target/vmlinux.debug"
    helper="$out/vmlinux-gdb.py"
    [[ -x "$gdb" ]] || die "Python-enabled GDB is not built; run download and build"
    [[ -s "$vmlinux" ]] || die "debug vmlinux is missing; run: ./viros.sh kernel-debug $target"
    [[ -s "$helper" ]] || die "output-tree Linux GDB Python extension is missing; rebuild $target"
    exec "$gdb" -nx -q -iex 'set auto-load safe-path /' "$vmlinux" \
        -ex "source $helper" \
        -ex 'set remotetimeout 10' \
        -ex "target remote $remote"
}

DEBUG_QEMU_PID=
DEBUG_GDB_SOCKET=
cleanup_debug_qemu() {
    local count
    if [[ -n "$DEBUG_QEMU_PID" ]] && kill -0 "$DEBUG_QEMU_PID" 2>/dev/null; then
        kill "$DEBUG_QEMU_PID" 2>/dev/null || true
        for count in {1..20}; do
            kill -0 "$DEBUG_QEMU_PID" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$DEBUG_QEMU_PID" 2>/dev/null; then
            kill -KILL "$DEBUG_QEMU_PID" 2>/dev/null || true
        fi
    fi
    [[ -n "$DEBUG_QEMU_PID" ]] && wait "$DEBUG_QEMU_PID" 2>/dev/null || true
    [[ -n "$DEBUG_GDB_SOCKET" ]] && rm -f -- "$DEBUG_GDB_SOCKET"
    DEBUG_QEMU_PID=
    DEBUG_GDB_SOCKET=
}

debug_qemu_failed() {
    local qemu_log=$1 console_log=$2
    printf 'viros.sh: QEMU exited before the debug stop\n' >&2
    [[ -s "$qemu_log" ]] && tail -n 30 "$qemu_log" >&2
    [[ -s "$console_log" ]] && tail -n 30 "$console_log" >&2
    return 1
}

wait_for_debug_socket() {
    local qemu_log=$1 console_log=$2 deadline=$((SECONDS + DEBUG_BOOT_TIMEOUT))
    until [[ -S "$DEBUG_GDB_SOCKET" ]]; do
        kill -0 "$DEBUG_QEMU_PID" 2>/dev/null || debug_qemu_failed "$qemu_log" "$console_log"
        (( SECONDS < deadline )) || die "timed out waiting for QEMU's GDB socket"
        sleep 0.1
    done
}

wait_for_console_pattern() {
    local pattern=$1 qemu_log=$2 console_log=$3 deadline=$((SECONDS + DEBUG_BOOT_TIMEOUT))
    until [[ -s "$console_log" ]] && grep -q -- "$pattern" "$console_log"; do
        kill -0 "$DEBUG_QEMU_PID" 2>/dev/null || debug_qemu_failed "$qemu_log" "$console_log"
        if (( SECONDS >= deadline )); then
            [[ -s "$console_log" ]] && tail -n 40 "$console_log" >&2
            die "timed out waiting for init to start (console pattern: $pattern)"
        fi
        sleep 0.2
    done
}

debug_stage() {
    local target=${1:-} qemu kernel initrd bios= out vmlinux helper console_log qemu_log gdb status init_entry=
    local -a qemu_args gdb_args
    case "$target" in
        arm|arm64|smips|ppc-e500-smp|ppc-440) ;;
        mipsbe|mmips)
            die "$target is not implemented: its RouterBOARD/MT7621 platform must reach /init before it can be offered as a debug target"
            ;;
        *) die "debug requires a validated target: arm, arm64, smips, ppc-e500-smp, or ppc-440" ;;
    esac
    need mkfs.ext2; need truncate
    prepare_one "$target"
    out="$BUILD/kernel-$target"
    vmlinux="$ARTIFACTS/$target/vmlinux.debug"
    helper="$out/vmlinux-gdb.py"
    gdb="$TOOLS/gdb/bin/gdb"
    [[ -x "$gdb" ]] || die "Python-enabled GDB is missing; run build"
    [[ -s "$vmlinux" && -s "$helper" ]] || die "matching kernel/debug helpers are missing; run: ./viros.sh kernel-debug $target"
    mkdir -p "$ARTIFACTS/$target" "$BUILD"
    DEBUG_GDB_SOCKET="$BUILD/gdb-$target-$$.sock"
    ((${#DEBUG_GDB_SOCKET} <= 100)) || die "GDB Unix socket path is too long; use a shorter VIROS_WORKDIR"
    console_log="$ARTIFACTS/$target/debug-console.log"
    qemu_log="$ARTIFACTS/$target/debug-qemu.log"
    : > "$console_log"
    : > "$qemu_log"

    case "$target" in
        arm)
            qemu=$(qemu_binary qemu-system-arm)
            kernel="$ARTIFACTS/arm/kernel.debug.zImage"
            initrd="$ARTIFACTS/arm/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug zImage is missing; run: ./viros.sh kernel-debug arm"
            qemu_args=( -M virt,gic-version=2 -cpu cortex-a15 -m 512M -smp 1
                -display none -monitor none -serial none -nic none -no-reboot -no-shutdown -S
                -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1 nokaslr' )
            ;;
        arm64)
            qemu=$(qemu_binary qemu-system-aarch64)
            kernel="$ARTIFACTS/arm64/kernel.debug.Image"
            initrd="$ARTIFACTS/arm64/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug Image is missing; run: ./viros.sh kernel-debug arm64"
            qemu_args=( -M virt -cpu cortex-a57 -m 512M -smp 2
                -display none -monitor none -serial none -nic none -no-reboot -no-shutdown -S
                -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyAMA0,115200 earlycon=pl011,0x09000000 loglevel=8 ignore_loglevel init=/init panic=-1 nokaslr' )
            ;;
        smips)
            qemu="$TOOLS/qemu/bin/qemu-system-mips"
            kernel="$ARTIFACTS/smips/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/smips/initramfs.cpio"
            [[ -x "$qemu" ]] || die "SMIPS requires viros's MetaROUTER-patched QEMU; run build"
            [[ -s "$kernel" ]] || die "debug vmlinux is missing; run: ./viros.sh kernel-debug smips"
            [[ -s "$ARTIFACTS/smips/init.entry" ]] || die "SMIPS init entry is missing; re-extract smips"
            init_entry=$(head -n 1 "$ARTIFACTS/smips/init.entry")
            [[ "$init_entry" =~ ^0x[0-9a-fA-F]+$ ]] || die "invalid SMIPS init entry: $init_entry"
            qemu_args=( -M malta -cpu 24Kc -m 256M
                -display none -monitor none -serial null -parallel none -nic none
                -no-reboot -no-shutdown -nodefaults -S -kernel "$kernel" -initrd "$initrd"
                -append 'board=vm mem=256M HZ=100000000 init=/init panic=-1' )
            ;;
        ppc-e500-smp)
            qemu=$(qemu_binary qemu-system-ppc)
            kernel="$ARTIFACTS/$target/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/$target/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug QEMU ELF is missing; run: ./viros.sh kernel-debug $target"
            qemu_args=( -M ppce500 -cpu e500v2 -smp 1 -m 256M
                -display none -monitor none -serial "file:$console_log" -nic none
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyS0 root=/dev/ram0' )
            ;;
        ppc-440)
            qemu=$(qemu_binary qemu-system-ppc)
            kernel="$ARTIFACTS/$target/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/$target/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug QEMU ELF is missing; run: ./viros.sh kernel-debug $target"
            bios=$(prepare_ppc440_dtb)
            qemu_args=( -L "$bios" -M sam460ex -cpu 460exb -m 256M
                -display none -monitor none -serial "file:$console_log" -nic none
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyS0 root=/dev/ram0' )
            ;;
    esac
    qemu_args+=( -gdb "unix:path=$DEBUG_GDB_SOCKET,server=on,wait=off" )
    "$qemu" "${qemu_args[@]}" 2> "$qemu_log" &
    DEBUG_QEMU_PID=$!
    trap cleanup_debug_qemu EXIT INT TERM
    wait_for_debug_socket "$qemu_log" "$console_log"

    case "$target" in
        ppc-*) wait_for_console_pattern 'Attempted to kill init' "$qemu_log" "$console_log" ;;
    esac

    gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
        -ex 'set pagination off' -ex 'set confirm off' -ex 'set remotetimeout 10'
        -ex "target remote $DEBUG_GDB_SOCKET" )
    if [[ "$target" == smips ]]; then
        gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
            -ex 'set pagination off' -ex 'set confirm off' -ex 'set remotetimeout 10' -ex 'set architecture mips'
            -ex "target remote $DEBUG_GDB_SOCKET" -ex 'hbreak start_thread' -ex continue )
    fi
    if [[ "$target" == arm || "$target" == arm64 ]]; then
        gdb_args+=( -ex 'hbreak ret_to_user' -ex continue )
    fi
    gdb_args+=( -ex "source $helper" -ex lx-version -ex lx-ps
        -ex 'p $lx_task_by_pid(1)->pid' -ex 'p $lx_task_by_pid(1)->comm' )
    if [[ "$target" == smips ]]; then
        gdb_args+=( -ex 'p $lx_task_by_pid(1)->mm->exe_file->f_path.dentry->d_name.name'
            -ex 'delete breakpoints' -ex "hbreak *$init_entry" -ex continue )
    fi
    say "Starting exact-symbol GDB for $target; PID 1 must be printed before the prompt"
    set +e
    "$gdb" "${gdb_args[@]}"
    status=$?
    set -e
    cleanup_debug_qemu
    trap - EXIT INT TERM
    return "$status"
}

list_stage() {
    cat <<'EOF'
Target          QEMU machine       Status
x86             q35                pending strict init + GDB validation
arm             virt/cortex-a15    success: PID 1 inspected with matching Python GDB
arm64           virt/cortex-a57    success: PID 1 inspected with matching Python GDB
mipsbe          no board model      unfinished: RB400 hardware, no init
mmips           no board model      unfinished: MT7621 hardware, no init
smips           Malta/board=vm      success: PID 1 inspected with matching Python GDB
ppc-e500-smp    ppce500/e500v2     success: PID 1 inspected with matching Python GDB
ppc-e500        —                  failed: ppce500 does not reach init
ppc-440         sam460ex/460EX      success: PID 1 inspected with matching Python GDB
ppc-83xx        —                  blocked: no MPC83xx QEMU machine
tile            TILE KVM-only      blocked: no TCG and unfinished GDB stub
EOF
}

doctor_stage() {
    local failed=0 command
    for command in bash curl python3 tar xz make gcc g++ pkg-config ninja mkfs.ext2 truncate unzip sha256sum flex bison bc perl dtc sed patch; do
        if command -v "$command" >/dev/null 2>&1; then
            printf 'ok       %s\n' "$command"
        else
            printf 'missing  %s\n' "$command"
            failed=1
        fi
    done
    if command -v arm-linux-gnueabi-gcc-11 >/dev/null 2>&1 || command -v arm-linux-gnueabi-gcc >/dev/null 2>&1; then
        printf 'ok       %s\n' 'ARM EABI cross compiler'
    else
        printf 'missing  %s\n' 'arm-linux-gnueabi-gcc (needed for ARM debug kernel)'
        failed=1
    fi
    if [[ $(uname -m) == aarch64 ]] || command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
        printf 'ok       %s\n' 'AArch64 kernel compiler'
    else
        printf 'missing  %s\n' 'aarch64-linux-gnu-gcc (needed off AArch64)'
        failed=1
    fi
    if command -v mips-linux-gnu-gcc >/dev/null 2>&1; then
        printf 'ok       %s\n' 'big-endian MIPS cross compiler'
    else
        printf 'missing  %s\n' 'mips-linux-gnu-gcc (needed for SMIPS debug kernel)'
        failed=1
    fi
    return "$failed"
}

main() {
    local command=${1:-help}
    shift || true
    case "$command" in
        download) download_stage "$@" ;;
        build) build_stage "$@" ;;
        kernel-debug) build_debug_kernel "$@" ;;
        extract) extract_stage "$@" ;;
        prepare) prepare_stage "$@" ;;
        run) run_stage "$@" ;;
        gdb) gdb_stage "$@" ;;
        debug) debug_stage "$@" ;;
        list) list_stage ;;
        doctor) doctor_stage ;;
        help|-h|--help) usage ;;
        *) usage >&2; die "unknown subcommand: $command" ;;
    esac
}

main "$@"
