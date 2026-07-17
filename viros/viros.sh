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
GMP_VERSION=${GMP_VERSION:-6.3.0}
GMP_SHA256=${GMP_SHA256:-a3c2b80201b89e68616f4ad30bc66aee4927c3ce50e33929ca819d5c43538898}
MPFR_VERSION=${MPFR_VERSION:-4.2.2}
MPFR_SHA256=${MPFR_SHA256:-b67ba0383ef7e8a8563734e2e889ef5ec3c3b898a01d00fa0a6869ad81c6ce01}
TILE_QEMU_VERSION=${TILE_QEMU_VERSION:-5.2.0}
LINUX_VERSION=${LINUX_VERSION:-5.6.3}
ROUTEROS_VERSION=${ROUTEROS_VERSION:-latest}
ROUTEROS_NPK=${ROUTEROS_NPK:-}
MIKROTIK_GPL_COMMIT=${MIKROTIK_GPL_COMMIT:-c3e110db1d35886c96ee14e16fc5a06bcac59692}
PPC_TOOLCHAIN_NAME=${PPC_TOOLCHAIN_NAME:-powerpc-e500mc--glibc--bleeding-edge-2020.08-1}
PPC_TOOLCHAIN_SHA256=${PPC_TOOLCHAIN_SHA256:-8cab4fbb645be782a6eaeb7b6afd75fda4c0dc8ca9a4095b0be9b6eeb29a9759}
MMIPS_TOOLCHAIN_NAME=${MMIPS_TOOLCHAIN_NAME:-mips32el--musl--stable-2020.08-1}
MMIPS_TOOLCHAIN_SHA256=${MMIPS_TOOLCHAIN_SHA256:-02155c88e0bf92f63105803767ce457790bfd920297ef326c9920853b5a3fe20}
X86_TOOLCHAIN_NAME=${X86_TOOLCHAIN_NAME:-x86-64-core-i7--glibc--bleeding-edge-2020.08-1}
X86_TOOLCHAIN_SHA256=${X86_TOOLCHAIN_SHA256:-77935109bbd1bdb84813a588b807052823033ed9094131fdd56f558023a3de08}
ARM_TOOLCHAIN_NAME=${ARM_TOOLCHAIN_NAME:-armv5-eabi--glibc--bleeding-edge-2020.08-1}
ARM_TOOLCHAIN_SHA256=${ARM_TOOLCHAIN_SHA256:-261e73520fb211f63a88ecce0689d3647acf295527bd6bd16e88e1bd65b3c603}
AARCH64_TOOLCHAIN_NAME=${AARCH64_TOOLCHAIN_NAME:-aarch64--glibc--bleeding-edge-2020.08-1}
AARCH64_TOOLCHAIN_SHA256=${AARCH64_TOOLCHAIN_SHA256:-212f3c05f3b2263b0e2f902d055aecc2755eba10c0011927788a38faee8fc9aa}
MIPSBE_TOOLCHAIN_NAME=${MIPSBE_TOOLCHAIN_NAME:-mips32--glibc--bleeding-edge-2020.08-1}
MIPSBE_TOOLCHAIN_SHA256=${MIPSBE_TOOLCHAIN_SHA256:-63baffcf0a94d7f1b7421ad61ddb56ce5c05595acd09f482dffe13ddf17efd81}
PIP_WHEEL_NAME=${PIP_WHEEL_NAME:-pip-26.1.2-py3-none-any.whl}
PIP_WHEEL_SHA256=${PIP_WHEEL_SHA256:-382ff9f685ee3bc25864f820aa50505825f10f5458ffff07e30a6d96e5715cab}
SETUPTOOLS_WHEEL_NAME=${SETUPTOOLS_WHEEL_NAME:-setuptools-83.0.0-py3-none-any.whl}
SETUPTOOLS_WHEEL_SHA256=${SETUPTOOLS_WHEEL_SHA256:-29b23c360f22f414dc7336bb39178cc7bcbf6021ed2733cde173f09dba19abb3}
PACKAGING_WHEEL_NAME=${PACKAGING_WHEEL_NAME:-packaging-26.0-py3-none-any.whl}
PACKAGING_WHEEL_SHA256=${PACKAGING_WHEEL_SHA256:-b36f1fef9334a5588b4166f8bcd26a14e521f2b55e6b9de3aaa80d3ff7a37529}
WHEEL_WHEEL_NAME=${WHEEL_WHEEL_NAME:-wheel-0.46.3-py3-none-any.whl}
WHEEL_WHEEL_SHA256=${WHEEL_WHEEL_SHA256:-4b399d56c9d9338230118d705d9737a2a468ccca63d5e813e2a4fc7815d8bc4d}
DTC_COMMIT=${DTC_COMMIT:-b6910bec11614980a21e46fbccc35934b671bd81}
DTC_SHA256=${DTC_SHA256:-e115f987eec23a1ba25150a46ced1675de3716072d3b4905afb3a9cda0f007c7}
UV_VERSION=${UV_VERSION:-0.11.21}
UV_PYTHON_VERSION=${UV_PYTHON_VERSION:-3.12.13}
UV_AARCH64_SHA256=${UV_AARCH64_SHA256:-88e800834007cc5efd4675f166eb2a51e7e3ad19876d85fa8805a6fb5c922397}
UV_X86_64_SHA256=${UV_X86_64_SHA256:-8c88519b0ef0af9801fcdee419bbb12116bd9e6b18e162ae093c932d8b264050}
JOBS=${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '2')}
DISK_SIZE=${DISK_SIZE:-64M}
DEBUG_BOOT_TIMEOUT=${DEBUG_BOOT_TIMEOUT:-30}

ARCHES=(x86 arm arm64 mipsbe mmips smips ppc tile)
RUN_TARGETS=(x86 arm arm64 mipsbe mmips smips ppc-e500-smp ppc-e500 ppc-440 ppc-83xx tile)

say() { printf '==> %s\n' "$*"; }
die() { printf 'viros.sh: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required host command not found: $1"; }

workdir_is_case_sensitive() {
    local check="$BUILD/.case-check-$$"
    mkdir -p "$check" || return 1
    : > "$check/probe"
    if [[ -e "$check/PROBE" ]]; then
        rm -rf -- "$check"
        return 1
    fi
    rm -rf -- "$check"
}

host_is_supported() {
    [[ $(uname -s) == Linux && $(getconf LONG_BIT 2>/dev/null || printf 0) == 64 ]] || return 1
    [[ $(uname -m) == x86_64 || $(uname -m) == aarch64 ]]
}

uv_host_target() {
    case "$(uname -m)" in
        x86_64) printf 'x86_64-unknown-linux-gnu\n' ;;
        aarch64) printf 'aarch64-unknown-linux-gnu\n' ;;
        *) die "no pinned uv binary for Linux/$(uname -m)" ;;
    esac
}

uv_host_sha256() {
    case "$(uname -m)" in
        x86_64) printf '%s\n' "$UV_X86_64_SHA256" ;;
        aarch64) printf '%s\n' "$UV_AARCH64_SHA256" ;;
        *) die "no pinned uv checksum for Linux/$(uname -m)" ;;
    esac
}

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
  gdb <target> [remote] Launch debug workflow, or attach to explicit remote
  debug <target>        Boot matching debug kernel, stop after init, open GDB

Information:
  list                  Print accepted run targets and their current status
  doctor                Check host prerequisites

Configuration is via QEMU_VERSION, GDB_VERSION, ROUTEROS_VERSION,
ROUTEROS_NPK, JOBS, DISK_SIZE, and VIROS_WORKDIR.  All output remains inside
VIROS_WORKDIR.
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

extract_zip_image() {
    local archive=$1 destination=$2 python
    python=$(managed_python)
    "$python" - "$archive" "$destination" <<'PY'
import shutil
import sys
import zipfile

archive, destination = sys.argv[1:]
with zipfile.ZipFile(archive) as source:
    images = [name for name in source.namelist() if name.endswith(".img") and not name.endswith("/")]
    if len(images) != 1:
        raise SystemExit(f"expected one .img in {archive}, found {len(images)}")
    with source.open(images[0]) as src, open(destination, "wb") as dst:
        shutil.copyfileobj(src, dst)
PY
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
    local uv_target
    host_is_supported || die "download requires x86-64 or AArch64 Linux"
    need curl
    mkdir -p "$DOWNLOADS"
    uv_target=$(uv_host_target)
    download_file "https://releases.astral.sh/github/uv/releases/download/${UV_VERSION}/uv-${uv_target}.tar.gz" \
        "$DOWNLOADS/uv-${UV_VERSION}-${uv_target}.tar.gz"
    download_file "https://download.qemu.org/qemu-${QEMU_VERSION}.tar.xz" "$DOWNLOADS/qemu-${QEMU_VERSION}.tar.xz"
    download_file "https://ftp.gnu.org/gnu/gdb/gdb-${GDB_VERSION}.tar.xz" "$DOWNLOADS/gdb-${GDB_VERSION}.tar.xz"
    download_file "https://ftp.gnu.org/gnu/gmp/gmp-${GMP_VERSION}.tar.xz" "$DOWNLOADS/gmp-${GMP_VERSION}.tar.xz"
    download_file "https://ftp.gnu.org/gnu/mpfr/mpfr-${MPFR_VERSION}.tar.xz" "$DOWNLOADS/mpfr-${MPFR_VERSION}.tar.xz"
    download_file "https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-${LINUX_VERSION}.tar.xz" "$DOWNLOADS/linux-${LINUX_VERSION}.tar.xz"
    download_file "https://files.pythonhosted.org/packages/5d/95/6b5cb3461ea5673ba0995989746db58eb18b91b54dbf331e72f569540946/${PIP_WHEEL_NAME}" "$DOWNLOADS/${PIP_WHEEL_NAME}"
    download_file "https://files.pythonhosted.org/packages/5d/40/e1e72872c6354b306daef1703549e8e83b4d43cfea356311bf722a043752/${SETUPTOOLS_WHEEL_NAME}" "$DOWNLOADS/${SETUPTOOLS_WHEEL_NAME}"
    download_file "https://files.pythonhosted.org/packages/b7/b9/c538f279a4e237a006a2c98387d081e9eb060d203d8ed34467cc0f0b9b53/${PACKAGING_WHEEL_NAME}" "$DOWNLOADS/${PACKAGING_WHEEL_NAME}"
    download_file "https://files.pythonhosted.org/packages/87/22/b76d483683216dde3d67cba61fb2444be8d5be289bf628c13fc0fd90e5f9/${WHEEL_WHEEL_NAME}" "$DOWNLOADS/${WHEEL_WHEEL_NAME}"
    download_file "https://gitlab.com/qemu-project/dtc/-/archive/${DTC_COMMIT}/dtc-${DTC_COMMIT}.tar.gz" "$DOWNLOADS/dtc-${DTC_COMMIT}.tar.gz"
    # Last upstream release containing TILE-Gx translation.  It is built for
    # linux-user analysis; it is not presented as a full-system emulator.
    download_file "https://download.qemu.org/qemu-${TILE_QEMU_VERSION}.tar.xz" "$DOWNLOADS/qemu-${TILE_QEMU_VERSION}-tile-legacy.tar.xz"
    download_file "https://github.com/tikoci/mikrotik-gpl/archive/${MIKROTIK_GPL_COMMIT}.tar.gz" "$DOWNLOADS/mikrotik-gpl-${MIKROTIK_GPL_COMMIT}.tar.gz"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/x86-64-core-i7/tarballs/${X86_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${X86_TOOLCHAIN_NAME}.tar.bz2"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/armv5-eabi/tarballs/${ARM_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${ARM_TOOLCHAIN_NAME}.tar.bz2"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/aarch64/tarballs/${AARCH64_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${AARCH64_TOOLCHAIN_NAME}.tar.bz2"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/mips32/tarballs/${MIPSBE_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${MIPSBE_TOOLCHAIN_NAME}.tar.bz2"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/powerpc-e500mc/tarballs/${PPC_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${PPC_TOOLCHAIN_NAME}.tar.bz2"
    download_file "https://toolchains.bootlin.com/downloads/releases/toolchains/mips32el/tarballs/${MMIPS_TOOLCHAIN_NAME}.tar.bz2" "$DOWNLOADS/${MMIPS_TOOLCHAIN_NAME}.tar.bz2"
    download_managed_python
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
    local src="$SOURCES/qemu-${QEMU_VERSION}" out="$BUILD/qemu-${QEMU_VERSION}" python
    setup_build_python
    python=$BUILD_PYTHON
    [[ -s "$DOWNLOADS/dtc-${DTC_COMMIT}.tar.gz" ]] || die "QEMU dtc/libfdt source is missing; run download first"
    verify_file "$DTC_SHA256" "$DOWNLOADS/dtc-${DTC_COMMIT}.tar.gz"
    unpack_source "$DOWNLOADS/dtc-${DTC_COMMIT}.tar.gz" "$src/subprojects/dtc"
    unpack_source "$DOWNLOADS/qemu-${QEMU_VERSION}.tar.xz" "$src"
    if [[ ! -f "$src/.routeros-arm-load" ]]; then
        grep -Eq '^#define KERNEL_LOAD_ADDR[[:space:]]+0x00010000$' "$src/hw/arm/boot.c" ||
            die "QEMU ARM raw load constant changed; cannot apply RouterOS TEXT_OFFSET fix"
        sed -i -E 's/^(#define KERNEL_LOAD_ADDR)[[:space:]]+0x00010000$/\1 0x00048000/' "$src/hw/arm/boot.c"
        : > "$src/.routeros-arm-load"
    fi
    if [[ ! -f "$src/.routeros-mips-vm" ]] ||
       ! grep -q 'MT7621 exposes its coherence manager' "$src/hw/mips/malta.c"; then
        say "Applying the RouterOS MetaROUTER/MMIPS Malta boot patch"
        patch --batch --forward -d "$src" -p1 < "$SCRIPT_DIR/qemu-mips-routeros.patch" ||
            die "RouterOS MIPS QEMU patch failed"
        : > "$src/.routeros-mips-vm"
    fi
    if [[ ! -f "$src/.routeros-mips-debug" ]] ||
       ! grep -q 'mmu_idx != MMU_KERNEL_IDX' "$src/target/mips/system/physaddr.c"; then
        say "Adding the RouterOS MIPS debug-memory and console support"
        patch --batch --forward -d "$src" -p1 < "$SCRIPT_DIR/qemu-mips-debug.patch" ||
            die "RouterOS MIPS debug QEMU patch failed"
        : > "$src/.routeros-mips-debug"
    fi
    if [[ ! -f "$src/.routeros-ppc-hypercall" ]]; then
        say "Applying the RouterOS e500 yield compatibility patch"
        patch --batch --forward -d "$src" -p1 < "$SCRIPT_DIR/qemu-ppc-routeros.patch" ||
            die "RouterOS PowerPC QEMU patch failed"
        : > "$src/.routeros-ppc-hypercall"
    fi
    mkdir -p "$out" "$TOOLS/qemu" "$DOWNLOADS/pip-cache"
    say "Configuring QEMU $QEMU_VERSION"
    (cd "$out" && env -u PYTHONPATH \
        PYTHONNOUSERSITE=1 PIP_CACHE_DIR="$DOWNLOADS/pip-cache" \
        "$src/configure" \
        --python="$python" \
        --prefix="$TOOLS/qemu" \
        --target-list=x86_64-softmmu,x86_64-linux-user,arm-softmmu,aarch64-softmmu,mips-softmmu,mipsel-softmmu,ppc-softmmu,ppc64-softmmu \
        --disable-download \
        -Dfdt=internal \
        --disable-docs --disable-gtk --disable-sdl --disable-vnc \
        --disable-linux-aio --disable-linux-io-uring \
        --disable-curl --disable-libssh --disable-rbd --disable-glusterfs)
    say "Building QEMU"
    make -C "$out" -j "$JOBS"
    make -C "$out" install
}

build_gmp() {
    local archive="$DOWNLOADS/gmp-${GMP_VERSION}.tar.xz"
    local src="$SOURCES/gmp-${GMP_VERSION}" out="$BUILD/gmp-${GMP_VERSION}"
    local prefix="$TOOLS/gmp-${GMP_VERSION}"
    [[ -s "$archive" ]] || die "GMP source is missing; run download first"
    verify_file "$GMP_SHA256" "$archive"
    if [[ -s "$prefix/include/gmp.h" && -s "$prefix/lib/libgmp.a" ]]; then
        return
    fi
    unpack_source "$archive" "$src"
    mkdir -p "$out" "$prefix"
    say "Building project-local GMP $GMP_VERSION"
    (cd "$out" && CFLAGS="${CFLAGS:--O2} -std=gnu17" \
        "$src/configure" --prefix="$prefix" --disable-shared --enable-static)
    make -C "$out" -j "$JOBS"
    make -C "$out" install
}

build_mpfr() {
    local archive="$DOWNLOADS/mpfr-${MPFR_VERSION}.tar.xz"
    local src="$SOURCES/mpfr-${MPFR_VERSION}" out="$BUILD/mpfr-${MPFR_VERSION}"
    local prefix="$TOOLS/mpfr-${MPFR_VERSION}" gmp_prefix="$TOOLS/gmp-${GMP_VERSION}"
    [[ -s "$archive" ]] || die "MPFR source is missing; run download first"
    verify_file "$MPFR_SHA256" "$archive"
    build_gmp
    if [[ -s "$prefix/include/mpfr.h" && -s "$prefix/lib/libmpfr.a" ]]; then
        return
    fi
    unpack_source "$archive" "$src"
    mkdir -p "$out" "$prefix"
    say "Building project-local MPFR $MPFR_VERSION"
    (cd "$out" && CFLAGS="${CFLAGS:--O2} -std=gnu17" \
        "$src/configure" --prefix="$prefix" --with-gmp="$gmp_prefix" \
        --disable-shared --enable-static)
    make -C "$out" -j "$JOBS"
    make -C "$out" install
}

build_gdb() {
    local src="$SOURCES/gdb-${GDB_VERSION}"
    local out="$BUILD/gdb-${GDB_VERSION}-python-${UV_PYTHON_VERSION}-localdeps" python python_prefix
    local gmp_prefix="$TOOLS/gmp-${GMP_VERSION}" mpfr_prefix="$TOOLS/mpfr-${MPFR_VERSION}"
    build_mpfr
    unpack_source "$DOWNLOADS/gdb-${GDB_VERSION}.tar.xz" "$src"
    mkdir -p "$out" "$TOOLS/gdb"
    python=$(managed_python)
    python_prefix=$("$python" -c 'import sys; print(sys.prefix)')
    say "Configuring Python-enabled multi-target GDB $GDB_VERSION"
    (cd "$out" && LDFLAGS="-L$python_prefix/lib -Wl,-rpath,$python_prefix/lib${LDFLAGS:+ $LDFLAGS}" "$src/configure" \
        --prefix="$TOOLS/gdb" --enable-targets=all --with-python="$python" \
        --with-gmp="$gmp_prefix" --with-mpfr="$mpfr_prefix" \
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

UV=
setup_uv() {
    local target sha archive destination="$TOOLS/uv-$UV_VERSION"
    target=$(uv_host_target)
    sha=$(uv_host_sha256)
    archive="$DOWNLOADS/uv-${UV_VERSION}-${target}.tar.gz"
    [[ -s "$archive" ]] || die "pinned uv is missing; run download first"
    verify_file "$sha" "$archive"
    if [[ ! -f "$destination/.unpacked" ]]; then
        mkdir -p "$destination"
        tar -xf "$archive" -C "$destination" --strip-components=1
        : > "$destination/.unpacked"
    fi
    UV="$destination/uv"
    [[ -x "$UV" ]] || die "uv executable was not found in $archive"
}

download_managed_python() {
    setup_uv
    say "Installing pinned managed CPython $UV_PYTHON_VERSION below VIROS_WORKDIR"
    UV_CACHE_DIR="$DOWNLOADS/uv-cache" \
    UV_PYTHON_INSTALL_DIR="$TOOLS/python" \
    UV_PYTHON_INSTALL_BIN=0 \
        "$UV" python install --managed-python --no-bin --no-progress "$UV_PYTHON_VERSION"
}

managed_python() {
    setup_uv
    UV_CACHE_DIR="$DOWNLOADS/uv-cache" \
    UV_PYTHON_INSTALL_DIR="$TOOLS/python" \
    UV_PYTHON_DOWNLOADS=never \
        "$UV" python find --managed-python --no-python-downloads "$UV_PYTHON_VERSION"
}

BUILD_PYTHON=
setup_build_python() {
    local python bootstrap="$BUILD/qemu-python-bootstrap" wheel_file
    local -a python_wheels=(
        "$PIP_WHEEL_NAME:$PIP_WHEEL_SHA256"
        "$SETUPTOOLS_WHEEL_NAME:$SETUPTOOLS_WHEEL_SHA256"
        "$PACKAGING_WHEEL_NAME:$PACKAGING_WHEEL_SHA256"
        "$WHEEL_WHEEL_NAME:$WHEEL_WHEEL_SHA256"
    )
    setup_uv
    python=$(managed_python)
    for wheel_file in "${python_wheels[@]}"; do
        local wheel_name=${wheel_file%%:*} wheel_sha=${wheel_file#*:}
        [[ -s "$DOWNLOADS/$wheel_name" ]] ||
            die "pinned Python bootstrap wheel is missing: $wheel_name; run download first"
        verify_file "$wheel_sha" "$DOWNLOADS/$wheel_name"
    done
    UV_CACHE_DIR="$DOWNLOADS/uv-cache" UV_PYTHON_DOWNLOADS=never \
        "$UV" venv --no-project --no-python-downloads --clear --python "$python" "$bootstrap"
    UV_CACHE_DIR="$DOWNLOADS/uv-cache" UV_PYTHON_DOWNLOADS=never \
        "$UV" pip install --python "$bootstrap/bin/python" --no-index \
        "$DOWNLOADS/$PIP_WHEEL_NAME" "$DOWNLOADS/$SETUPTOOLS_WHEEL_NAME" \
        "$DOWNLOADS/$PACKAGING_WHEEL_NAME" "$DOWNLOADS/$WHEEL_WHEEL_NAME"
    BUILD_PYTHON="$bootstrap/bin/python"
}

unpack_toolchain() {
    local name=$1 expected=$2 key=$3 label=$4
    local archive="$DOWNLOADS/${name}.tar.bz2" destination="$TOOLS/cross-$key"
    [[ -s "$archive" ]] || die "$label toolchain is missing; run download first"
    verify_file "$expected" "$archive"
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

X86_CROSS_PREFIX=
ARM_CROSS_PREFIX=
AARCH64_CROSS_PREFIX=
MIPSBE_CROSS_PREFIX=
PPC_CROSS_PREFIX=
MMIPS_CROSS_PREFIX=

setup_downloaded_cross() {
    local name=$1 expected=$2 key=$3 triplet=$4 output_name=$5 gcc_version=$6 label=$7
    local destination="$TOOLS/cross-$key" real_bin compiler wrapper_bin tool compiler_version cross_prefix x86_loader x86_root
    unpack_toolchain "$name" "$expected" "$key" "$label"
    compiler=$(find "$destination" -path "*/bin/$triplet-gcc" -print -quit)
    [[ -n "$compiler" ]] || die "$triplet-gcc was not found in the pinned $label toolchain"
    real_bin=$(dirname -- "$compiler")
    if [[ $(uname -m) == x86_64 ]]; then
        cross_prefix="$real_bin/$triplet-"
    else
        [[ -x "$TOOLS/qemu/bin/qemu-x86_64" ]] ||
            die "qemu-x86_64 is required to run the pinned $label compiler on $(uname -m); build QEMU first"
        # Bootlin's compiler programs are x86-64, but non-x86 target sysroots
        # naturally contain only their target loaders.  The pinned x86
        # toolchain provides one common x86-64 glibc runtime for every adapter.
        unpack_toolchain "$X86_TOOLCHAIN_NAME" "$X86_TOOLCHAIN_SHA256" x86 x86-64
        x86_loader=$(find "$TOOLS/cross-x86" -path '*/sysroot/lib/ld-linux-x86-64.so.2' -print -quit)
        [[ -n "$x86_loader" ]] || die "the pinned x86-64 toolchain has no host runtime loader"
        x86_root=${x86_loader%/lib/ld-linux-x86-64.so.2}
        wrapper_bin="$TOOLS/cross-$key-emulated/bin"
        mkdir -p "$wrapper_bin/gcc-tools"
        chmod +x "$SCRIPT_DIR/emulated-cross-tool"
        for tool in gcc ld as nm objcopy objdump strip ar ranlib readelf size strings; do
            ln -sfn "$SCRIPT_DIR/emulated-cross-tool" "$wrapper_bin/$triplet-$tool"
        done
        # GCC reports this path to Kbuild and collect2 executes it directly.
        ln -sfn "$SCRIPT_DIR/emulated-cross-tool" "$wrapper_bin/gcc-tools/ld"
        export VIROS_QEMU_X86_64="$TOOLS/qemu/bin/qemu-x86_64"
        export VIROS_X86_LD_ROOT="$x86_root"
        export VIROS_CROSS_REAL_BIN="$real_bin"
        export VIROS_CROSS_TRIPLET="$triplet"
        cross_prefix="$wrapper_bin/$triplet-"
    fi
    compiler_version=$("${cross_prefix}gcc" --version)
    [[ "${compiler_version%%$'\n'*}" == *"$gcc_version"* ]] ||
        die "the pinned $label compiler is not GCC $gcc_version: ${compiler_version%%$'\n'*}"
    printf -v "$output_name" '%s' "$cross_prefix"
}

setup_x86_cross() { setup_downloaded_cross "$X86_TOOLCHAIN_NAME" "$X86_TOOLCHAIN_SHA256" x86 x86_64-buildroot-linux-gnu X86_CROSS_PREFIX 10.2.0 x86-64; }
setup_arm_cross() { setup_downloaded_cross "$ARM_TOOLCHAIN_NAME" "$ARM_TOOLCHAIN_SHA256" arm arm-buildroot-linux-gnueabi ARM_CROSS_PREFIX 10.2.0 ARM; }
setup_aarch64_cross() { setup_downloaded_cross "$AARCH64_TOOLCHAIN_NAME" "$AARCH64_TOOLCHAIN_SHA256" aarch64 aarch64-buildroot-linux-gnu AARCH64_CROSS_PREFIX 10.2.0 AArch64; }
setup_mipsbe_cross() { setup_downloaded_cross "$MIPSBE_TOOLCHAIN_NAME" "$MIPSBE_TOOLCHAIN_SHA256" mipsbe mips-buildroot-linux-gnu MIPSBE_CROSS_PREFIX 10.2.0 MIPSBE; }
setup_ppc_cross() { setup_downloaded_cross "$PPC_TOOLCHAIN_NAME" "$PPC_TOOLCHAIN_SHA256" powerpc powerpc-linux PPC_CROSS_PREFIX 10.2.0 PowerPC; }
setup_mmips_cross() { setup_downloaded_cross "$MMIPS_TOOLCHAIN_NAME" "$MMIPS_TOOLCHAIN_SHA256" mmips mipsel-buildroot-linux-musl MMIPS_CROSS_PREFIX 9.3.0 MMIPS; }

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
    local target=${1:-} src out config arch prefix= image= obj raw compiler_version
    local -a targets
    host_is_supported || die "debug-kernel build requires x86-64 or AArch64 Linux"
    case "$target" in
        x86|arm|arm64|mipsbe|mmips|smips|ppc-e500-smp|ppc-e500|ppc-440) ;;
        *) die "no validated matching debug-kernel boot for $target" ;;
    esac
    workdir_is_case_sensitive ||
        die "Linux debug kernels require a case-sensitive VIROS_WORKDIR; $WORKDIR is case-insensitive"
    need flex; need bison; need bc; need perl
    prepare_mikrotik_kernel_source
    src=$(mikrotik_kernel_source)
    out="$BUILD/kernel-$target"
    case "$target" in
        x86)
            arch=x86_64
            config=$(find_kernel_config x86_64.config)
            setup_x86_cross; prefix=$X86_CROSS_PREFIX
            image=bzImage
            ;;
        arm)
            arch=arm
            config=$(find_kernel_config arm.config)
            setup_arm_cross; prefix=$ARM_CROSS_PREFIX
            image=zImage
            ;;
        arm64)
            arch=arm64
            config=$(find_kernel_config arm64.config aarch64.config)
            setup_aarch64_cross; prefix=$AARCH64_CROSS_PREFIX
            image=Image
            ;;
        mipsbe)
            arch=mips
            config=$(find_kernel_config mips.config)
            setup_mipsbe_cross; prefix=$MIPSBE_CROSS_PREFIX
            ;;
        mmips)
            arch=mips
            config=$(find_kernel_config mmips.config)
            setup_mmips_cross; prefix=$MMIPS_CROSS_PREFIX
            ;;
        smips)
            arch=mips
            config=$(find_kernel_config smips.config)
            setup_mipsbe_cross; prefix=$MIPSBE_CROSS_PREFIX
            ;;
        ppc-e500-smp)
            arch=powerpc; config=$(find_kernel_config e500-smp.config)
            setup_ppc_cross; prefix=$PPC_CROSS_PREFIX
            ;;
        ppc-e500)
            arch=powerpc; config=$(find_kernel_config e500.config)
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
    if [[ "$target" == x86 ]]; then
        # The published path names an out-of-tree build input.  The extractor
        # supplies that exact production initramfs to QEMU instead.
        "$src/scripts/config" --file "$out/.config" --set-str INITRAMFS_SOURCE '' \
            --enable KALLSYMS --enable KALLSYMS_ALL --enable IKCONFIG
    elif [[ "$target" == smips || "$target" == mipsbe || "$target" == mmips ]]; then
        "$src/scripts/config" --file "$out/.config" \
            --enable DEBUG_INFO_DWARF4 --enable KALLSYMS --enable KALLSYMS_ALL --enable IKCONFIG
    fi
    local make_args=( -C "$src" O="$out" ARCH="$arch" CROSS_COMPILE="$prefix" )
    say "Building patched Linux $LINUX_VERSION for $target with MikroTik's published config"
    make "${make_args[@]}" olddefconfig
    targets=(vmlinux scripts_gdb)
    [[ -n "$image" ]] && targets+=("$image")
    make "${make_args[@]}" -j "$JOBS" "${targets[@]}"
    cp -f -- "$out/vmlinux" "$ARTIFACTS/$target/vmlinux.debug"
    [[ -s "$out/vmlinux-gdb.py" ]] || die "kernel build did not create vmlinux-gdb.py"
    case "$target" in
        x86)
            cp -f -- "$out/arch/x86/boot/bzImage" "$ARTIFACTS/x86/kernel.debug.bzImage"
            ;;
        arm)
            cp -f -- "$out/arch/arm/boot/zImage" "$ARTIFACTS/arm/kernel.debug.zImage"
            ;;
        arm64)
            cp -f -- "$out/arch/arm64/boot/Image" "$ARTIFACTS/arm64/kernel.debug.Image"
            ;;
        smips|mmips)
            cp -f -- "$out/vmlinux" "$ARTIFACTS/$target/kernel.debug.qemu.elf"
            ;;
        mipsbe)
            # CONFIG_MAPPED_KERNEL links at 0xc0000000.  Shift only the ELF
            # load view so Malta places the bytes in RAM; the entry code
            # installs the disclosed c0000000 wired mapping before continuing.
            "$prefix"objcopy --change-addresses=-0x40000000 "$out/vmlinux" \
                "$ARTIFACTS/mipsbe/kernel.debug.qemu.elf"
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
        if [[ -n "$prefix" ]]; then
            compiler_version=$("$prefix"gcc --version)
        else
            compiler_version=$(gcc --version)
        fi
        printf '%s\n' "${compiler_version%%$'\n'*}"
    } > "$ARTIFACTS/$target/debug-build.provenance"
    say "Matching debug vmlinux: $ARTIFACTS/$target/vmlinux.debug"
}

build_tile_linux_user() {
    local src="$SOURCES/qemu-${TILE_QEMU_VERSION}-tile-legacy"
    local out="$BUILD/qemu-${TILE_QEMU_VERSION}-tile-legacy-python-${UV_PYTHON_VERSION}" python
    setup_build_python
    python=$BUILD_PYTHON
    unpack_source "$DOWNLOADS/qemu-${TILE_QEMU_VERSION}-tile-legacy.tar.xz" "$src"
    mkdir -p "$out" "$TOOLS/tile-legacy"
    say "Configuring legacy TILE-Gx linux-user translator"
    (cd "$out" && env -u PYTHONPATH PYTHONNOUSERSITE=1 \
        "$src/configure" --python="$python" --meson=internal \
        --prefix="$TOOLS/tile-legacy" \
        --target-list=tilegx-linux-user --disable-system --disable-docs \
        --disable-tools --disable-linux-aio --disable-linux-io-uring \
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
    host_is_supported || die "build requires x86-64 or AArch64 Linux"
    need make; need tar; need gcc; need g++; need pkg-config; need sha256sum
    [[ -s "$DOWNLOADS/qemu-${QEMU_VERSION}.tar.xz" ]] || die "run download first"
    build_qemu
    build_gdb
    build_tile_linux_user
    build_mikrotik_tile_kvm
    build_debug_kernel x86
    build_debug_kernel arm
    build_debug_kernel arm64
    build_debug_kernel mipsbe
    build_debug_kernel mmips
    build_debug_kernel smips
    build_debug_kernel ppc-e500-smp
    build_debug_kernel ppc-e500
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
    if [[ -n "$ROUTEROS_NPK" ]]; then
        [[ -s "$ROUTEROS_NPK" ]] || die "ROUTEROS_NPK is not a readable non-empty file: $ROUTEROS_NPK"
        printf '%s\n' "$ROUTEROS_NPK"
        return
    fi
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
    local arch=$1 npk python
    npk=$(npk_for_arch "$arch")
    say "Extracting $arch from $(basename -- "$npk")"
    python=$(managed_python)
    "$python" "$SCRIPT_DIR/npk_extract.py" "$npk" "$arch" "$ARTIFACTS"
}

extract_stage() {
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
            extract_zip_image "$zip" "$IMAGES/chr-${version}.img"
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

prepare_ppc_e500_dtb() {
    local qemu=$1 kernel=$2 initrd=$3 cmdline=$4 target=$5
    local directory="$ARTIFACTS/$target"
    local base="$ARTIFACTS/$target/ppce500-base.dtb"
    local dts="$ARTIFACTS/$target/rb1000.dts"
    local out="$ARTIFACTS/$target/rb1000.dtb"
    need dtc; need sed
    mkdir -p "$directory"

    # Preserve QEMU's load addresses for this exact kernel/initramfs pair.
    # Only select the RB1000 platform published in MikroTik's e500 kernel.
    rm -f -- "$base" "$dts" "$out"
    "$qemu" -M "ppce500,dumpdtb=$base" -cpu e500v2 -smp 1 -m 256M \
        -display none -monitor none -serial null -no-reboot -no-shutdown \
        -nodefaults -kernel "$kernel" -initrd "$initrd" -append "$cmdline" \
        -nic none >/dev/null
    [[ -s "$base" ]] || die "QEMU did not generate the ppce500 device tree"
    dtc -q -I dtb -O dts -o "$dts" "$base"
    sed -i '0,/compatible = "fsl,qemu-e500";/s//compatible = "RB1000";/' "$dts"
    sed -i '0,/model = "QEMU ppce500";/s//model = "RB1000";/' "$dts"
    grep -q 'compatible = "RB1000";' "$dts" || die "failed to select the RB1000 e500 platform"
    grep -q 'linux,initrd-start' "$dts" || die "generated e500 device tree lacks initrd placement"
    grep -q 'linux,initrd-end' "$dts" || die "generated e500 device tree lacks initrd extent"
    dtc -q -I dts -O dtb -o "$out" "$dts"
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

mipsbe_kernel_cmdline() {
    local initrd=$1 size physical mapped
    local ram_bytes=$((256 * 1024 * 1024)) alignment=$((64 * 1024)) overhead=$((128 * 1024))
    [[ -s "$initrd" ]] || die "MIPSBE initramfs is missing: $initrd"
    size=$(wc -c < "$initrd")
    (( size + overhead < ram_bytes )) || die "MIPSBE initramfs is too large for the 256 MiB Malta guest"

    # Malta rounds the initrd placement up to 64 KiB.  QEMU's normal PROM
    # argument uses a KSEG0 address, but MikroTik's CONFIG_MAPPED_KERNEL maps
    # RAM at 0xc0000000, so supply the same physical offset in that mapping.
    physical=$(( ((ram_bytes - size - overhead + alignment - 1) / alignment) * alignment ))
    mapped=$((0xc0000000 + physical))
    printf 'board=vm mem=256M HZ=100000000 console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1 rd_start=0x%x rd_size=%d\n' \
        "$mapped" "$size"
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
    esac

    prepare_one "$target"
    local qemu kernel initrd disk mips_cmdline ppc_cmdline dtb
    disk="$IMAGES/$target.raw"
    case "$target" in
        x86)
            qemu=$(qemu_binary qemu-system-x86_64)
            local version chr
            version=$(routeros_version); chr="$IMAGES/chr-${version}.img"
            [[ -s "$chr" ]] || die "CHR image missing; run download then prepare x86"
            exec "$qemu" -machine pc,accel=tcg -cpu qemu64 -m 512M -nographic -no-reboot \
                -drive "file=$chr,format=raw,if=ide" -nic none "$@"
            ;;
        arm)
            qemu=$(qemu_binary qemu-system-arm); kernel="$ARTIFACTS/arm/kernel.raw"; initrd="$ARTIFACTS/arm/initramfs.cpio"
            exec "$qemu" -M virt -cpu cortex-a15 -smp 1 -m 512M \
                -display none -monitor none -serial stdio -no-reboot \
                -d in_asm -D "$ARTIFACTS/arm/qemu-in_asm.log" \
                -kernel "$kernel" -initrd "$initrd" \
                -append 'console=ttyAMA0,115200 earlycon=pl011,0x09000000 loglevel=8 ignore_loglevel init=/init panic=-1' \
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
            qemu="$TOOLS/qemu/bin/qemu-system-mips"; kernel="$ARTIFACTS/mipsbe/kernel.qemu.elf"; initrd="$ARTIFACTS/mipsbe/initramfs.cpio"
            [[ -x "$qemu" ]] || die "MIPSBE requires viros's MetaROUTER-patched QEMU; run download and build"
            mips_cmdline=$(mipsbe_kernel_cmdline "$initrd")
            exec "$qemu" -M malta -cpu 24Kc -m 256M -display none -monitor none -serial stdio -parallel none \
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd" -append "$mips_cmdline" \
                -nic none "$@"
            ;;
        smips)
            qemu="$TOOLS/qemu/bin/qemu-system-mips"; kernel="$ARTIFACTS/smips/kernel.qemu.elf"; initrd="$ARTIFACTS/smips/initramfs.cpio"
            [[ -x "$qemu" ]] || die "SMIPS requires viros's MetaROUTER-patched QEMU; run download and build"
            exec "$qemu" -M malta -cpu 24Kc -m 256M -display none -monitor none -serial stdio \
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd" \
                -append 'board=vm mem=256M HZ=100000000 console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1' \
                -nic none "$@"
            ;;
        mmips)
            qemu="$TOOLS/qemu/bin/qemu-system-mipsel"; kernel="$ARTIFACTS/mmips/kernel.qemu.elf"; initrd="$ARTIFACTS/mmips/initramfs.cpio"
            [[ -x "$qemu" ]] || die "MMIPS requires viros's MT7621-compatible patched QEMU; run download and build"
            exec "$qemu" -M malta -cpu 34Kf -smp 1 -m 256M \
                -display none -monitor none -serial none -parallel none \
                -chardev stdio,id=mikrotik-mmips-uart,signal=off \
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd" \
                -append 'board=750g-mt mem=256M HZ=100000000 console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1' \
                -nic none "$@"
            ;;
        ppc-e500-smp)
            qemu=$(qemu_binary qemu-system-ppc); kernel="$ARTIFACTS/$target/kernel.qemu.elf"; initrd="$ARTIFACTS/$target/initramfs.cpio"
            # The SMP-flavoured RouterOS image is the only one compiled with
            # QEMU e500 platform support, but its secondary-core bring-up
            # stalls on current QEMU.  One vCPU reaches /init reliably.
            exec "$qemu" -M ppce500 -cpu e500v2 -smp 1 -m 256M -nographic -no-reboot \
                -nodefaults -serial stdio -monitor none \
                -kernel "$kernel" -initrd "$initrd" -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel root=/dev/ram0' \
                -nic none "$@"
            ;;
        ppc-e500)
            qemu=$(qemu_binary qemu-system-ppc)
            kernel="$ARTIFACTS/$target/kernel.qemu.elf"
            initrd="$ARTIFACTS/$target/initramfs.cpio"
            [[ -x "$TOOLS/qemu/bin/qemu-system-ppc" ]] ||
                die "PPC e500 requires viros's RouterOS-patched QEMU; run download and build"
            ppc_cmdline='console=ttyS0,115200 loglevel=8 ignore_loglevel root=/dev/ram0 init=/init panic=-1'
            dtb=$(prepare_ppc_e500_dtb "$qemu" "$kernel" "$initrd" "$ppc_cmdline" "$target")
            exec "$qemu" -M ppce500 -cpu e500v2 -smp 1 -m 256M -nographic -no-reboot \
                -nodefaults -serial stdio -monitor none -dtb "$dtb" \
                -kernel "$kernel" -initrd "$initrd" -append "$ppc_cmdline" \
                -nic none "$@"
            ;;
        ppc-440)
            local bios
            qemu=$(qemu_binary qemu-system-ppc); kernel="$ARTIFACTS/ppc-440/kernel.qemu.elf"; initrd="$ARTIFACTS/ppc-440/initramfs.cpio"
            bios=$(prepare_ppc440_dtb)
            exec "$qemu" -L "$bios" -M sam460ex -cpu 460exb -m 256M -nographic -no-reboot \
                -nodefaults -serial stdio -monitor none \
                -kernel "$kernel" -initrd "$initrd" -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel root=/dev/ram0' \
                -nic none "$@"
            ;;
    esac
}

gdb_stage() {
    local target=${1:-} remote gdb out vmlinux helper
    local -a gdb_args
    [[ -n "$target" ]] || die "gdb requires a target"
    if (($# == 1)); then
        debug_stage "$target"
        return
    fi
    remote=$2
    case "$target" in
        x86|arm|arm64|mipsbe|mmips|smips|ppc-e500-smp|ppc-e500|ppc-440) ;;
        *) die "no validated matching debug kernel for $target" ;;
    esac
    gdb="$TOOLS/gdb/bin/gdb"
    out="$BUILD/kernel-$target"
    vmlinux="$ARTIFACTS/$target/vmlinux.debug"
    helper="$out/vmlinux-gdb.py"
    [[ -x "$gdb" ]] || die "Python-enabled GDB is not built; run download and build"
    [[ -s "$vmlinux" ]] || die "debug vmlinux is missing; run: ./viros.sh kernel-debug $target"
    [[ -s "$helper" ]] || die "output-tree Linux GDB Python extension is missing; rebuild $target"
    gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
        -ex "source $helper" -ex 'set remotetimeout 10' )
    if [[ "$target" == mipsbe || "$target" == mmips || "$target" == smips ]]; then
        gdb_args+=( -ex 'set architecture mips' )
        [[ "$target" == mmips ]] && gdb_args+=( -ex 'set suppress-cli-notifications on' )
    elif [[ "$target" == x86 ]]; then
        gdb_args+=( -ex 'set architecture i386:x86-64' )
    fi
    exec "$gdb" "${gdb_args[@]}" -ex "target remote $remote"
}

DEBUG_QEMU_PID=
DEBUG_GDB_SOCKET=
DEBUG_CONSOLE_SOCKET=
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
    [[ -n "$DEBUG_CONSOLE_SOCKET" ]] && rm -f -- "$DEBUG_CONSOLE_SOCKET"
    DEBUG_QEMU_PID=
    DEBUG_GDB_SOCKET=
    DEBUG_CONSOLE_SOCKET=
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
    local target=${1:-} qemu kernel initrd bios= out vmlinux helper console_log qemu_log gdb status init_entry= mips_cmdline= ppc_cmdline= dtb= console_chardev
    local -a qemu_args gdb_args
    case "$target" in
        x86|arm|arm64|mipsbe|mmips|smips|ppc-e500-smp|ppc-e500|ppc-440) ;;
        *) die "debug requires a validated target: x86, arm, arm64, mipsbe, mmips, smips, ppc-e500-smp, ppc-e500, or ppc-440" ;;
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
    DEBUG_CONSOLE_SOCKET="$BUILD/console-$target-$$.sock"
    ((${#DEBUG_GDB_SOCKET} <= 100)) || die "GDB Unix socket path is too long; use a shorter VIROS_WORKDIR"
    ((${#DEBUG_CONSOLE_SOCKET} <= 100)) || die "console Unix socket path is too long; use a shorter VIROS_WORKDIR"
    console_log="$ARTIFACTS/$target/debug-console.log"
    qemu_log="$ARTIFACTS/$target/debug-qemu.log"
    : > "$console_log"
    : > "$qemu_log"

    case "$target" in
        x86)
            qemu=$(qemu_binary qemu-system-x86_64)
            kernel="$ARTIFACTS/x86/kernel.debug.bzImage"
            initrd="$ARTIFACTS/x86/initramfs.cpio"
            local version chr
            version=$(routeros_version); chr="$IMAGES/chr-${version}.img"
            [[ -s "$kernel" ]] || die "debug bzImage is missing; run: ./viros.sh kernel-debug x86"
            [[ -s "$initrd" ]] || die "x86 initramfs is missing; re-extract x86"
            [[ -s "$chr" ]] || die "CHR image missing; run download then prepare x86"
            [[ -s "$ARTIFACTS/x86/init.entry" ]] || die "x86 init entry is missing; re-extract x86"
            init_entry=$(head -n 1 "$ARTIFACTS/x86/init.entry")
            [[ "$init_entry" =~ ^0x[0-9a-fA-F]+$ ]] || die "invalid x86 init entry: $init_entry"
            qemu_args=( -machine pc,accel=tcg -cpu qemu64 -smp 1 -m 512M
                -display none -monitor none -nic none
                -no-reboot -no-shutdown -nodefaults -S
                -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel rdinit=/init init=/init panic=-1 nokaslr'
                -drive "file=$chr,format=raw,if=ide" )
            ;;
        arm)
            qemu=$(qemu_binary qemu-system-arm)
            kernel="$ARTIFACTS/arm/kernel.debug.zImage"
            initrd="$ARTIFACTS/arm/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug zImage is missing; run: ./viros.sh kernel-debug arm"
            qemu_args=( -M virt,gic-version=2 -cpu cortex-a15 -m 512M -smp 1
                -display none -monitor none -nic none -no-reboot -no-shutdown -S
                -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyAMA0,115200 earlycon=pl011,0x09000000 loglevel=8 ignore_loglevel init=/init panic=-1 nokaslr' )
            ;;
        arm64)
            qemu=$(qemu_binary qemu-system-aarch64)
            kernel="$ARTIFACTS/arm64/kernel.debug.Image"
            initrd="$ARTIFACTS/arm64/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug Image is missing; run: ./viros.sh kernel-debug arm64"
            qemu_args=( -M virt -cpu cortex-a57 -m 512M -smp 2
                -display none -monitor none -nic none -no-reboot -no-shutdown -S
                -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyAMA0,115200 earlycon=pl011,0x09000000 loglevel=8 ignore_loglevel init=/init panic=-1 nokaslr' )
            ;;
        mipsbe)
            qemu="$TOOLS/qemu/bin/qemu-system-mips"
            kernel="$ARTIFACTS/mipsbe/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/mipsbe/initramfs.cpio"
            [[ -x "$qemu" ]] || die "MIPSBE requires viros's MetaROUTER-patched QEMU; run build"
            [[ -s "$kernel" ]] || die "debug vmlinux is missing; run: ./viros.sh kernel-debug mipsbe"
            [[ -s "$ARTIFACTS/mipsbe/init.entry" ]] || die "MIPSBE init entry is missing; re-extract mipsbe"
            init_entry=$(head -n 1 "$ARTIFACTS/mipsbe/init.entry")
            [[ "$init_entry" =~ ^0x[0-9a-fA-F]+$ ]] || die "invalid MIPSBE init entry: $init_entry"
            mips_cmdline=$(mipsbe_kernel_cmdline "$initrd")
            qemu_args=( -M malta -cpu 24Kc -m 256M
                -display none -monitor none -parallel none -nic none
                -no-reboot -no-shutdown -nodefaults -S -kernel "$kernel" -initrd "$initrd"
                -append "$mips_cmdline" )
            ;;
        mmips)
            qemu="$TOOLS/qemu/bin/qemu-system-mipsel"
            kernel="$ARTIFACTS/mmips/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/mmips/initramfs.cpio"
            [[ -x "$qemu" ]] || die "MMIPS requires viros's MT7621-compatible patched QEMU; run build"
            [[ -s "$kernel" ]] || die "debug vmlinux is missing; run: ./viros.sh kernel-debug mmips"
            [[ -s "$ARTIFACTS/mmips/init.entry" ]] || die "MMIPS init entry is missing; re-extract mmips"
            init_entry=$(head -n 1 "$ARTIFACTS/mmips/init.entry")
            [[ "$init_entry" =~ ^0x[0-9a-fA-F]+$ ]] || die "invalid MMIPS init entry: $init_entry"
            qemu_args=( -M malta -cpu 34Kf -smp 1 -m 256M
                -display none -monitor none -serial none -parallel none -nic none
                -no-reboot -no-shutdown -nodefaults -S -kernel "$kernel" -initrd "$initrd"
                -append 'board=750g-mt mem=256M HZ=100000000 console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1' )
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
                -display none -monitor none -parallel none -nic none
                -no-reboot -no-shutdown -nodefaults -S -kernel "$kernel" -initrd "$initrd"
                -append 'board=vm mem=256M HZ=100000000 console=ttyS0,115200 loglevel=8 ignore_loglevel init=/init panic=-1' )
            ;;
        ppc-e500-smp)
            qemu=$(qemu_binary qemu-system-ppc)
            kernel="$ARTIFACTS/$target/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/$target/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug QEMU ELF is missing; run: ./viros.sh kernel-debug $target"
            qemu_args=( -M ppce500 -cpu e500v2 -smp 1 -m 256M
                -display none -monitor none -nic none
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel root=/dev/ram0' )
            ;;
        ppc-e500)
            qemu=$(qemu_binary qemu-system-ppc)
            [[ -x "$TOOLS/qemu/bin/qemu-system-ppc" ]] ||
                die "PPC e500 requires viros's RouterOS-patched QEMU; run build"
            kernel="$ARTIFACTS/$target/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/$target/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug QEMU ELF is missing; run: ./viros.sh kernel-debug $target"
            ppc_cmdline='console=ttyS0,115200 loglevel=8 ignore_loglevel root=/dev/ram0 init=/init panic=-1'
            dtb=$(prepare_ppc_e500_dtb "$qemu" "$kernel" "$initrd" "$ppc_cmdline" "$target")
            qemu_args=( -M ppce500 -cpu e500v2 -smp 1 -m 256M
                -display none -monitor none -nic none
                -no-reboot -no-shutdown -nodefaults -dtb "$dtb"
                -kernel "$kernel" -initrd "$initrd" -append "$ppc_cmdline" )
            ;;
        ppc-440)
            qemu=$(qemu_binary qemu-system-ppc)
            kernel="$ARTIFACTS/$target/kernel.debug.qemu.elf"
            initrd="$ARTIFACTS/$target/initramfs.cpio"
            [[ -s "$kernel" ]] || die "debug QEMU ELF is missing; run: ./viros.sh kernel-debug $target"
            bios=$(prepare_ppc440_dtb)
            qemu_args=( -L "$bios" -M sam460ex -cpu 460exb -m 256M
                -display none -monitor none -nic none
                -no-reboot -no-shutdown -nodefaults -kernel "$kernel" -initrd "$initrd"
                -append 'console=ttyS0,115200 loglevel=8 ignore_loglevel root=/dev/ram0' )
            ;;
    esac
    if [[ "$target" == mmips ]]; then
        console_chardev=mikrotik-mmips-uart
    else
        console_chardev=viros-console
    fi
    qemu_args+=( -chardev "socket,id=$console_chardev,path=$DEBUG_CONSOLE_SOCKET,server=on,wait=off,logfile=$console_log,logappend=on" )
    [[ "$target" == mmips ]] || qemu_args+=( -serial "chardev:$console_chardev" )
    qemu_args+=( -gdb "unix:path=$DEBUG_GDB_SOCKET,server=on,wait=off" )
    "$qemu" "${qemu_args[@]}" 2> "$qemu_log" &
    DEBUG_QEMU_PID=$!
    trap cleanup_debug_qemu EXIT INT TERM
    wait_for_debug_socket "$qemu_log" "$console_log"

    case "$target" in
        ppc-*) wait_for_console_pattern 'Attempted to kill init' "$qemu_log" "$console_log" ;;
    esac

    gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
        -ex 'set pagination off' -ex 'set confirm off' -ex 'set python print-stack full' -ex 'set remotetimeout 10'
        -ex "target remote $DEBUG_GDB_SOCKET" )
    if [[ "$target" == mmips ]]; then
        # CPS-backed MIPS starts with a topology refresh that briefly leaves
        # GDB without a selected CLI thread.  Suppress only those connection
        # notifications; breakpoints, registers, and Python helpers are live.
        gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
            -ex 'set pagination off' -ex 'set confirm off' -ex 'set python print-stack full' -ex 'set remotetimeout 10'
            -ex 'set suppress-cli-notifications on' -ex 'set architecture mips'
            -ex "target remote $DEBUG_GDB_SOCKET" -ex 'thbreak start_thread' -ex continue )
    elif [[ "$target" == mipsbe || "$target" == smips ]]; then
        gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
            -ex 'set pagination off' -ex 'set confirm off' -ex 'set python print-stack full' -ex 'set remotetimeout 10' -ex 'set architecture mips'
            -ex "target remote $DEBUG_GDB_SOCKET" -ex 'thbreak start_thread' -ex continue )
    elif [[ "$target" == x86 ]]; then
        gdb_args=( -nx -q -iex 'set auto-load safe-path /' "$vmlinux"
            -ex 'set pagination off' -ex 'set confirm off' -ex 'set python print-stack full' -ex 'set remotetimeout 10' -ex 'set architecture i386:x86-64'
            -ex "target remote $DEBUG_GDB_SOCKET" -ex 'tbreak compat_start_thread' -ex continue )
    fi
    if [[ "$target" == arm || "$target" == arm64 ]]; then
        gdb_args+=( -ex 'thbreak ret_to_user' -ex continue )
    fi
    gdb_args+=( -ex "source $helper" -ex lx-version -ex lx-ps
        -ex 'p $lx_task_by_pid(1)->pid' -ex 'p $lx_task_by_pid(1)->comm' )
    if [[ "$target" == mipsbe || "$target" == mmips || "$target" == smips || "$target" == x86 ]]; then
        gdb_args+=( -ex 'p $lx_task_by_pid(1)->mm->exe_file->f_path.dentry->d_name.name'
            -ex 'delete breakpoints' )
        if [[ "$target" == x86 ]]; then
            # QEMU x86 TCG does not advertise hardware breakpoints, but a
            # software breakpoint in the mapped IA32 init page is reliable.
            gdb_args+=( -ex "tbreak *$init_entry" -ex continue )
        else
            gdb_args+=( -ex "thbreak *$init_entry" -ex continue )
        fi
    fi
    gdb_args+=( -ex "source $SCRIPT_DIR/gdb_console.py" )
    say "QEMU PID: $DEBUG_QEMU_PID"
    say "GDB socket: $DEBUG_GDB_SOCKET"
    say "interactive console socket: $DEBUG_CONSOLE_SOCKET"
    say "VM console log: $console_log"
    say "QEMU diagnostic log: $qemu_log"
    if [[ "$target" == mipsbe || "$target" == mmips || "$target" == smips ]]; then
        say "Temporary breakpoint 1: kernel start_thread (prove and inspect PID 1)"
        say "Temporary breakpoint 2: RouterOS /init ELF entry $init_entry (final prompt)"
    fi
    say "At the GDB prompt run 'viros-console'; press Ctrl-] to return to GDB"
    say "Starting exact-symbol GDB for $target; PID 1 must be printed before the prompt"
    set +e
    VIROS_CONSOLE_SOCKET="$DEBUG_CONSOLE_SOCKET" VIROS_CONSOLE_LOG="$console_log" \
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
x86             PC/CHR             success: PID 1 inspected with matching Python GDB
arm             virt/cortex-a15    success: PID 1 inspected with matching Python GDB
arm64           virt/cortex-a57    success: PID 1 inspected with matching Python GDB
mipsbe          Malta/board=vm     success: PID 1 inspected with matching Python GDB
mmips           Malta/MT7621       success: PID 1 inspected with matching Python GDB
smips           Malta/board=vm     success: PID 1 inspected with matching Python GDB
ppc-e500-smp    ppce500/e500v2     success: PID 1 inspected with matching Python GDB
ppc-e500        ppce500/RB1000     success: PID 1 inspected with matching Python GDB
ppc-440         sam460ex/460EX      success: PID 1 inspected with matching Python GDB
ppc-83xx        —                  blocked: no MPC83xx QEMU machine
tile            TILE KVM-only      blocked: no TCG and unfinished GDB stub
EOF
}

doctor_stage() {
    local failed=0 command host_bits name python uv_target
    for command in bash curl tar xz make gcc g++ m4 pkg-config ninja mkfs.ext2 truncate sha256sum flex bison bc perl dtc sed patch; do
        if command -v "$command" >/dev/null 2>&1; then
            printf 'ok       %s\n' "$command"
        else
            printf 'missing  %s\n' "$command"
            failed=1
        fi
    done
    host_bits=$(getconf LONG_BIT 2>/dev/null || printf 0)
    if host_is_supported; then
        printf 'ok       64-bit Linux/%s build host\n' "$(uname -m)"
    else
        printf 'missing  x86-64 or AArch64 Linux build host (found %s-bit %s/%s)\n' "$host_bits" "$(uname -s)" "$(uname -m)"
        failed=1
    fi
    if host_is_supported; then
        uv_target=$(uv_host_target)
        python=$(find "$TOOLS/python" -path "*/bin/python${UV_PYTHON_VERSION%.*}" -print -quit 2>/dev/null || true)
        if [[ -x "$TOOLS/uv-$UV_VERSION/uv" && -x "$python" ]] &&
            "$python" -c "import sys; assert sys.version.split()[0] == '$UV_PYTHON_VERSION'" >/dev/null 2>&1; then
            printf 'ready    uv %s with managed CPython %s\n' "$UV_VERSION" "$UV_PYTHON_VERSION"
        else
            printf 'download uv %s and managed CPython %s (supplied by ./viros.sh download for %s)\n' \
                "$UV_VERSION" "$UV_PYTHON_VERSION" "$uv_target"
        fi
    fi
    if workdir_is_case_sensitive; then
        printf 'ok       case-sensitive VIROS_WORKDIR\n'
    else
        printf 'missing  case-sensitive VIROS_WORKDIR (required by the Linux source tree; found %s)\n' "$WORKDIR"
        failed=1
    fi
    for name in "$X86_TOOLCHAIN_NAME" "$ARM_TOOLCHAIN_NAME" "$AARCH64_TOOLCHAIN_NAME" \
        "$MIPSBE_TOOLCHAIN_NAME" "$MMIPS_TOOLCHAIN_NAME" "$PPC_TOOLCHAIN_NAME"; do
        if [[ -s "$DOWNLOADS/$name.tar.bz2" ]]; then
            printf 'ready    %s\n' "$name"
        else
            printf 'download %s (supplied by ./viros.sh download)\n' "$name"
        fi
    done
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
