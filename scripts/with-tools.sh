#!/usr/bin/env bash
set -euo pipefail

# Resolve mount aliases and symlinks once so compiler paths written into CMake
# caches do not oscillate between equivalent spellings of this checkout.
repo=$(cd "$(dirname "$0")/.." && pwd -P)
tools=$repo/.tools
tool_state=$tools
if [[ $(uname -s) == Darwin ]]; then
    tool_arch=$(uname -m)
    [[ $tool_arch == arm64 ]] && tool_arch=aarch64
    # A checkout may be used from both a Linux builder and macOS. uv-managed
    # tools are native executables, so Darwin must not reuse Linux's existing
    # .tools state. Keep Linux's established paths unchanged.
    tool_state=$tools/host-macos-$tool_arch
    # Homebrew's rustup otherwise writes toolchains into the user's global
    # ~/.rustup. Keep the pinned Darwin toolchain local to this checkout.
    export RUSTUP_HOME=${RUSTUP_HOME:-$tool_state/rustup}
    export SARUN_SWIPL_CACHE=${SARUN_SWIPL_CACHE:-$tool_state/swipl-cache}
    # Zig opens the final link's many Rust objects concurrently. macOS's
    # default soft limit is too low for the engine test binary.
    ulimit -n 4096 2>/dev/null || true
fi
bin=$tool_state/bin
uv_version=0.11.21

mkdir -p "$bin"

# Keep uv-managed executables and Python installations local even when the uv
# executable itself comes from the host. Its download cache remains in the
# ordinary XDG cache because it is safe and useful to share between checkouts.
export UV_TOOL_DIR=$tool_state/uv-tools
export UV_TOOL_BIN_DIR=$tool_state/uv-tools-bin
export UV_PYTHON_INSTALL_DIR=$tool_state/uv-python
# uv exposes cargo-zigbuild in UV_TOOL_BIN_DIR, while ziglang's `zig` helper
# remains beside it in the managed tool environment.
export PATH=$UV_TOOL_DIR/cargo-zigbuild/bin:$UV_TOOL_BIN_DIR:$PATH
export CARGO_ZIGBUILD_CACHE_DIR=$tool_state/cargo-zigbuild-cache
export CARGO_ZIGBUILD_ZIG_PATH=$UV_TOOL_DIR/cargo-zigbuild/bin/python-zig
export ZIG_GLOBAL_CACHE_DIR=$tool_state/zig-cache

if ! command -v uv >/dev/null 2>&1; then
    if [[ ! -x $bin/uv ]]; then
        command -v curl >/dev/null 2>&1 || {
            echo "sarun bootstrap needs curl to install uv" >&2
            exit 1
        }
        installer=$tools/uv-installer.sh
        curl --proto '=https' --tlsv1.2 -fsSL \
            "https://astral.sh/uv/$uv_version/install.sh" -o "$installer"
        UV_UNMANAGED_INSTALL="$bin" sh "$installer"
        rm -f "$installer"
    fi
    export PATH=$bin:$PATH
fi

# A distribution-provided rustup is fine: rust-toolchain.toml supplies the
# repository override, so no global default is needed. If rustup is absent,
# install only its proxies locally; the pinned compiler and rustfmt component
# are then resolved by that same repository override.
if ! command -v rustup >/dev/null 2>&1; then
    export RUSTUP_HOME=$tool_state/rustup
    export CARGO_HOME=$tool_state/cargo
    if [[ ! -x $CARGO_HOME/bin/rustup ]]; then
        command -v curl >/dev/null 2>&1 || {
            echo "sarun bootstrap needs curl to install rustup" >&2
            exit 1
        }
        installer=$tools/rustup-init.sh
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$installer"
        sh "$installer" -y --no-modify-path --profile minimal --default-toolchain none
        rm -f "$installer"
    fi
    export PATH=$CARGO_HOME/bin:$PATH
fi

# Homebrew can install `rustup` beside independent Homebrew `cargo`/`rustc`
# binaries. In that layout the repository override is silently bypassed. Make
# local rustup proxies so every nested shell (including Makefile recipes) uses
# rust-toolchain.toml without changing the user's global Rust installation.
rustup_proxy_dir=$tool_state/rustup-proxies
mkdir -p "$rustup_proxy_dir"
rustup_executable=$(command -v rustup)
for proxy in cargo rustc rustdoc rustfmt cargo-fmt clippy-driver; do
    ln -sfn "$rustup_executable" "$rustup_proxy_dir/$proxy"
done
export PATH=$rustup_proxy_dir:$PATH

if (( $# == 0 )); then
    printf 'uv: %s\nrustup: %s\n' "$(command -v uv)" "$(command -v rustup)"
    exit 0
fi

exec "$@"
