#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
java_bin=${JAVA:-java}
tlc_jar=${TLC_JAR:-"$repo_root/formal/tools/tla2tools.jar"}
if [[ "$tlc_jar" != /* ]]; then
  tlc_jar="$repo_root/$tlc_jar"
fi

if [[ ! -f "$tlc_jar" ]]; then
  cat >&2 <<EOF
formal model check requires a pinned TLA+ tools jar.
Set TLC_JAR to an existing tla2tools.jar (or place it at
$repo_root/formal/tools/tla2tools.jar); the command never downloads it.
EOF
  exit 2
fi
if ! command -v "$java_bin" >/dev/null 2>&1; then
  echo "formal model check requires a Java runtime; set JAVA to its executable" >&2
  exit 2
fi
if ! "$java_bin" -version >/dev/null 2>&1; then
  echo "the configured Java executable cannot start a runtime: $java_bin" >&2
  exit 2
fi

run_model() {
  local module=$1
  local config=$2
  echo "→ SANY $module"
  "$java_bin" -cp "$tlc_jar" tla2sany.SANY "$repo_root/$module"
  echo "→ TLC $module"
  "$java_bin" -cp "$tlc_jar" tlc2.TLC -deadlock -workers 1 \
    -config "$repo_root/$config" "$repo_root/$module"
}

run_model formal/mirror_run/MirrorRun.tla \
  formal/mirror_run/MirrorRun.cfg
run_model formal/generation_publication/GenerationPublication.tla \
  formal/generation_publication/GenerationPublication.cfg
