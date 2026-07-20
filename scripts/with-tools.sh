#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
tools=$repo/.tools
bin=$tools/bin
uv_version=0.11.21

mkdir -p "$bin"

# Keep uv-managed executables and Python installations local even when the uv
# executable itself comes from the host. Its download cache remains in the
# ordinary XDG cache because it is safe and useful to share between checkouts.
export UV_TOOL_DIR=$tools/uv-tools
export UV_TOOL_BIN_DIR=$tools/uv-tools-bin
export UV_PYTHON_INSTALL_DIR=$tools/uv-python
# uv exposes cargo-zigbuild in UV_TOOL_BIN_DIR, while ziglang's `zig` helper
# remains beside it in the managed tool environment.
export PATH=$UV_TOOL_DIR/cargo-zigbuild/bin:$UV_TOOL_BIN_DIR:$PATH

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
    export RUSTUP_HOME=$tools/rustup
    export CARGO_HOME=$tools/cargo
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

if (( $# == 0 )); then
    printf 'uv: %s\nrustup: %s\n' "$(command -v uv)" "$(command -v rustup)"
    exit 0
fi

exec "$@"
