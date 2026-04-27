#!/bin/sh
# run_tree.sh — parent script that invokes all three test program types.
# Called by the test harness; DIR is set by the caller.
DIR="$(dirname "$0")"
"$DIR/hello_static"
"$DIR/hello_dynamic"
"$DIR/hello_shebang.sh"
