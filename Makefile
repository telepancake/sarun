# Makefile — the commands for this repo. `make` (no args) lists every target;
# each is documented with the `##` comment on its rule line.
#
# `make engine` builds the fully-static musl binary and drops a `./sarun`
# symlink at the repo root pointing at it (.gitignore'd). `make run` execs that
# symlink. `prototype/libtestsarun.py` is NOT a program — it is the test-support
# library the engine tests import (wire client + sqlar readers); there is no
# Python app to run.

SHELL := bash
.DEFAULT_GOAL := help

# ---- Discovery ------------------------------------------------------------

.PHONY: help
help: ## Show this help (the list of every command)
	@echo "sarun — available make targets:"
	@echo
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  \033[1mmake %-14s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo
	@echo "Run 'make <target>'. See CLAUDE.md for deeper context."

# ---- App ------------------------------------------------------------------

.PHONY: run
run: ## Start sarun (the engine binary + UI; build it first with `make engine`)
	@if [ -x ./sarun ]; then exec ./sarun; \
	else echo "no ./sarun — build it with 'make engine'"; exit 1; fi

# ---- System dependencies --------------------------------------------------

.PHONY: deps
deps: ## Install system packages (FUSE, bubblewrap; iproute2 + tshark for net tests)
	apt-get install -y libfuse3-dev fuse3 pkg-config bubblewrap gcc
	apt-get install -y iproute2 tshark   # only needed by test_net_rs.py

# ---- Build ----------------------------------------------------------------
#
# The only build: a fully-static musl binary via cargo-zigbuild + ziglang (no
# apt toolchain). The cargo default target is musl (engine/.cargo/config.toml).

.PHONY: vendor
vendor: ## Assemble engine/vendor/ from pinned upstreams + vendor-patches/ series
	python3 scripts/vendor.py

.PHONY: engine
engine: vendor ## Build the engine (fully-static musl binary; cargo-zigbuild + zig)
	@command -v uv >/dev/null || { echo "engine needs uv (https://docs.astral.sh/uv/)"; exit 1; }
	uv tool install --with ziglang cargo-zigbuild
	rustup target add x86_64-unknown-linux-musl
	cd engine && PATH="$$(uv tool dir)/cargo-zigbuild/bin:$$HOME/.local/bin:$$PATH" \
	  cargo zigbuild --release --target x86_64-unknown-linux-musl
	@ln -sfn engine/target/x86_64-unknown-linux-musl/release/sarun sarun
	@echo "→ ./sarun → engine/target/x86_64-unknown-linux-musl/release/sarun"

# ---- Tests ----------------------------------------------------------------
#
# The tests drive the engine binary (build it first) and import
# prototype/libtestsarun.py for the wire client + sqlar readers. test_oci.py is
# heavy and hermetic, so it has its own target.

.PHONY: test
test: ## Run the test suite (pytest-xdist; build the engine first; excludes test_oci.py + the box corpus)
	cd prototype && uv run --with pytest --with pytest-xdist --with pytest-timeout \
	  --with "wcmatch>=8.4" --with "python-magic>=0.4" --with "pyte>=0.8" \
	  pytest -q -p no:cacheprovider -n auto --dist=loadscope \
	  --timeout=180 --timeout-method=signal --ignore=test_oci.py \
	  --ignore=test_kati_corpus_box_rs.py

.PHONY: test-oci
test-oci: ## Run the hermetic OCI tests (synthetic archive; real engine; needs `make engine`)
	prototype/test_oci.py

.PHONY: test-kati-box
test-kati-box: ## The FULL kati conformance corpus through real -b boxes vs GNU make (needs `make engine`; ~10 min)
	cd prototype && uv run --with "pyfuse3>=3.2" --with "trio>=0.22" \
	  --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_kati_corpus_box_rs.py

.PHONY: test-contract
test-contract: ## Syscall-level (strace) contract test for the native builtins (needs `make engine` + strace)
	uv run --with pytest python engine/test_builtin_contract.py

# ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (engine/target/, ./sarun symlink, __pycache__)
	rm -rf engine/target
	rm -f sarun
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
