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

# The addin set the sarun runner requires of the sud wrappers.
SUD_ADDINS := sud/trace sud/path_remap sud/cmd-rewrite sud/fake-exec sud/inramfs
HOST_ARCH := $(shell uname -m)
ENGINE_TARGET ?= $(HOST_ARCH)-unknown-linux-musl
SWIPL_TARGET := $(subst -unknown,,$(ENGINE_TARGET))
ENGINE_RELEASE := engine/target/$(ENGINE_TARGET)/release

.PHONY: engine
engine: vendor wire-codegen ## Build the engine (fully-static musl binary; cargo-zigbuild + zig)
	@command -v uv >/dev/null || { echo "engine needs uv (https://docs.astral.sh/uv/)"; exit 1; }
	uv tool install --with ziglang cargo-zigbuild
	rustup target add $(ENGINE_TARGET)
	cd engine && PATH="$$(uv tool dir)/cargo-zigbuild/bin:$$HOME/.local/bin:$$PATH" \
	  cargo zigbuild --release --target $(ENGINE_TARGET)
	@ln -sfn $(ENGINE_RELEASE)/sarun sarun
	@# sud is the DEFAULT run backend: the wrappers must sit next to the
	@# engine binary (runner::sud_wrapper_paths resolves the sibling).
	@# sud64 and sud32 are both required: the wrappers hand off to each
	@# other on cross-class execs, so they MUST come from the same build.
	@# tv/Makefile uses zig's bundled musl+UAPI headers, so no host -m32
	@# toolchain is required; fail visibly instead of leaving stale/missing
	@# wrapper siblings in the release directory.
	$(MAKE) -C tv sud64 sud32 SUD_ADDINS="$(SUD_ADDINS)"
	cp tv/sud64 tv/sud32 $(ENGINE_RELEASE)/
	@# The mirror drivers are compiled INTO sarun (multi-call dispatch on
	@# argv[0] / subcommand — mirrors.rs self-execs); the symlinks are a
	@# convenience for invoking a driver by name from the build dir.
	@for d in gitdepot wikimak ietfmak; do \
	  ln -sf sarun $(ENGINE_RELEASE)/$$d; done
	@echo "→ ./sarun → $(ENGINE_RELEASE)/sarun"

.PHONY: swipl
swipl: ## Build pinned static SWI-Prolog + zlib artifacts (cached outside the repo)
	@command -v uv >/dev/null || { echo "swipl needs uv (https://docs.astral.sh/uv/)"; exit 1; }
	uv tool install --with ziglang cargo-zigbuild
	PATH="$$(uv tool dir)/cargo-zigbuild/bin:$$HOME/.local/bin:$$PATH" \
	  uv run --with cmake --with ninja python3 scripts/swipl.py --target $(SWIPL_TARGET)

.PHONY: wire-codegen
wire-codegen: swipl ## Project concrete Rust transport codecs from the Prolog relation
	python3 scripts/wire_codegen.py

.PHONY: check-wire-codegen
check-wire-codegen: swipl ## Fail if the checked-in Rust transport projection is stale
	python3 scripts/wire_codegen.py --check

.PHONY: test-action-grammar
test-action-grammar: swipl ## Run the core-only action grammar tests with pinned host SWI-Prolog
	@shopt -s nullglob; \
	  bins=( "$${XDG_CACHE_HOME:-$$HOME/.cache}"/sarun/swipl/9.2.9/pipeline-*/$$(uname -m)/native-swipl-build/src/swipl ); \
	  (( $${#bins[@]} )) || { echo "pinned host swipl not found after make swipl"; exit 1; }; \
	  "$${bins[-1]}" -q -f none -s engine/pl/test_grammar_ir.pl \
	    -g test_grammar_ir:run_grammar_ir_tests -t halt; \
	  "$${bins[-1]}" -q -f none -s engine/pl/test_grammar_engine.pl \
	    -g test_grammar_engine:run_grammar_engine_tests -t halt; \
	  "$${bins[-1]}" -q -f none -s engine/pl/test_action_grammar.pl \
	    -g test_action_grammar:run_action_grammar_tests -t halt; \
	  "$${bins[-1]}" -q -f none -s engine/pl/test_context_relation.pl \
	    -g test_context_relation:run_context_relation_tests -t halt; \
	  "$${bins[-1]}" -q -f none -s engine/pl/test_transport_catalog.pl \
	    -g test_transport_catalog:run_transport_catalog_tests -t halt

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

.PHONY: test-integ
test-integ: ## Real-project builds (GNU hello autoconf + cmake) through -b boxes (needs `make engine`; also in `make test`)
	cd prototype && uv run --with "pyfuse3>=3.2" --with "trio>=0.22" \
	  --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_integration_builds_rs.py

.PHONY: test-contract
test-contract: ## Syscall-level (strace) contract test for the native builtins (needs `make engine` + strace)
	uv run --with pytest python engine/test_builtin_contract.py

.PHONY: test-sud
test-sud: ## sud vs FUSE equivalence + sud exec capabilities (needs `make engine`; builds sud64/sud32)
	cd prototype && uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_sud_equiv_rs.py
	cd prototype && uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_sud_concurrent_rs.py

# ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (engine/target/, ./sarun symlink, __pycache__)
	rm -rf engine/target
	rm -f sarun
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
