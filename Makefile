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
TOOLS_RUN := $(CURDIR)/scripts/with-tools.sh

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
# Linux releases remain fully-static musl binaries built through cargo-zigbuild.
# macOS releases are native Mach-O binaries so they can use Hypervisor.framework
# and the Darwin host primitives; the Linux appliance init is still cross-built
# separately by `make appliances`.

.PHONY: vendor
vendor: ## Assemble engine/vendor/ from pinned upstreams + vendor-patches/ series
	python3 scripts/vendor.py

.PHONY: tools
tools: ## Bootstrap repository-local uv/rustup fallbacks and cargo-zigbuild
	$(TOOLS_RUN) uv tool install --with ziglang cargo-zigbuild

# The addin set the sarun runner requires of the sud wrappers.
HOST_OS := $(shell uname -s)
HOST_ARCH_RAW := $(shell uname -m)
HOST_ARCH := $(if $(filter arm64,$(HOST_ARCH_RAW)),aarch64,$(HOST_ARCH_RAW))
ifeq ($(HOST_OS),Darwin)
ENGINE_TARGET ?= $(HOST_ARCH)-apple-darwin
SWIPL_TARGET := $(ENGINE_TARGET)
else
ENGINE_TARGET ?= $(HOST_ARCH)-unknown-linux-musl
SWIPL_TARGET := $(subst -unknown,,$(ENGINE_TARGET))
endif
ENGINE_RELEASE := engine/target/$(ENGINE_TARGET)/release

.PHONY: engine
engine: vendor wire-codegen tools ## Build the host engine (native on macOS; static musl on Linux)
	$(TOOLS_RUN) rustup target add $(ENGINE_TARGET)
ifeq ($(HOST_OS),Darwin)
	$(TOOLS_RUN) cargo build --release --target $(ENGINE_TARGET) \
	  --manifest-path engine/Cargo.toml
else
	$(TOOLS_RUN) bash -c 'cd engine && \
	  cargo zigbuild --release --target $(ENGINE_TARGET)'
	@# SUD is Linux-only. Both wrappers must sit next to the Linux engine and
	@# must come from the same build because they hand off on cross-class execs.
	$(TOOLS_RUN) $(MAKE) -C tv sud64 sud32
	cp tv/sud64 tv/sud32 $(ENGINE_RELEASE)/
endif
	@ln -sfn $(ENGINE_RELEASE)/sarun sarun
	$(TOOLS_RUN) python3 scripts/release_licenses.py --target $(ENGINE_TARGET) \
	  --output $(ENGINE_RELEASE)/LICENSES
	@# The mirror drivers are compiled INTO sarun (multi-call dispatch on
	@# argv[0] / subcommand — mirrors.rs self-execs); the symlinks are a
	@# convenience for invoking a driver by name from the build dir.
	@for d in gitdepot wikimak ietfmak; do \
	  ln -sf sarun $(ENGINE_RELEASE)/$$d; done
	@echo "→ ./sarun → $(ENGINE_RELEASE)/sarun"

.PHONY: appliances
appliances: tools ## Build both tightly paired QEMU + Linux + target-/init appliances
	$(TOOLS_RUN) scripts/build-appliances.sh all

.PHONY: release-licenses
release-licenses: vendor ## Regenerate notices beside the current static release
	$(TOOLS_RUN) python3 scripts/release_licenses.py --target $(ENGINE_TARGET) \
	  --output $(ENGINE_RELEASE)/LICENSES

.PHONY: swipl
swipl: tools ## Build pinned static SWI-Prolog + zlib artifacts (cached outside the repo)
	$(TOOLS_RUN) uv run --with 'cmake==4.2.3' --with ninja \
	  python3 scripts/swipl.py --target $(SWIPL_TARGET)

.PHONY: wire-codegen
wire-codegen: swipl ## Project concrete Rust transport codecs from the Prolog relation
	$(TOOLS_RUN) python3 scripts/wire_codegen.py

.PHONY: check-wire-codegen
check-wire-codegen: swipl ## Fail if the checked-in Rust transport projection is stale
	$(TOOLS_RUN) python3 scripts/wire_codegen.py --check

.PHONY: check-models
check-models: ## Run the bounded TLA+/TLC design models (requires user-provided TLC_JAR + Java)
	@scripts/check-formal-models.sh

.PHONY: test-action-grammar
test-action-grammar: swipl ## Run the core-only action grammar tests with pinned host SWI-Prolog
	@swipl=$$(find "$${SARUN_SWIPL_CACHE:-$${XDG_CACHE_HOME:-$$HOME/.cache}/sarun/swipl/9.2.9}" \
	    .tools/host-macos-$$(uname -m | sed 's/^arm64$$/aarch64/')/swipl-cache \
	    -path '*/native-swipl-build/src/swipl' -type f 2>/dev/null | sort | tail -1); \
	  [ -n "$$swipl" ] || { echo "pinned host swipl not found after make swipl"; exit 1; }; \
	  $$swipl -q -f none -s engine/pl/test_relation_api.pl \
	    -g test_relation_api:run_relation_api_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_grammar_ir.pl \
	    -g test_grammar_ir:run_grammar_ir_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_grammar_engine.pl \
	    -g test_grammar_engine:run_grammar_engine_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_brush_grammar.pl \
	    -g test_brush_grammar:run_brush_grammar_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_action_grammar.pl \
	    -g test_action_grammar:run_action_grammar_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_context_relation.pl \
	    -g test_context_relation:run_context_relation_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_local_state_relation.pl \
	    -g test_local_state_relation:run_local_state_relation_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_ast_state_relation.pl \
	    -g test_ast_state_relation:run_ast_state_relation_tests -t halt; \
	  $$swipl -q -f none -s engine/pl/test_transport_catalog.pl \
	    -g test_transport_catalog:run_transport_catalog_tests -t halt

# ---- Tests ----------------------------------------------------------------
#
# The tests drive the engine binary (build it first) and import
# prototype/libtestsarun.py for the wire client + sqlar readers. test_oci.py is
# heavy and hermetic, so it has its own target.

.PHONY: test
test: ## Run the test suite (pytest-xdist; build the engine first; excludes test_oci.py + the box corpus)
	cd prototype && $(TOOLS_RUN) uv run --with pytest --with pytest-xdist --with pytest-timeout \
	  --with "wcmatch>=8.4" --with "python-magic>=0.4" --with "pyte>=0.8" \
	  pytest -q -p no:cacheprovider -n auto --dist=loadscope \
	  --timeout=180 --timeout-method=signal --ignore=test_oci.py \
	  --ignore=test_kati_corpus_box_rs.py

.PHONY: test-oci
test-oci: ## Run the hermetic OCI tests (synthetic archive; real engine; needs `make engine`)
	prototype/test_oci.py

.PHONY: test-kati-box
test-kati-box: ## The FULL kati conformance corpus through real -b boxes vs GNU make (needs `make engine`; ~10 min)
	cd prototype && $(TOOLS_RUN) uv run --with "pyfuse3>=3.2" --with "trio>=0.22" \
	  --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_kati_corpus_box_rs.py

.PHONY: test-integ
test-integ: ## Real-project builds (GNU hello autoconf + cmake) through -b boxes (needs `make engine`; also in `make test`)
	cd prototype && $(TOOLS_RUN) uv run --with "pyfuse3>=3.2" --with "trio>=0.22" \
	  --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_integration_builds_rs.py

.PHONY: test-contract
test-contract: ## Syscall-level (strace) contract test for the native builtins (needs `make engine` + strace)
	$(TOOLS_RUN) uv run --with pytest python engine/test_builtin_contract.py

.PHONY: test-sud
test-sud: ## sud vs FUSE equivalence + sud exec capabilities (needs `make engine`; builds sud64/sud32)
	cd prototype && $(TOOLS_RUN) uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_sud_equiv_rs.py
	cd prototype && $(TOOLS_RUN) uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_sud_concurrent_rs.py

.PHONY: test-backends
test-backends: ## Portable live SarunFs equivalence (FUSE, host QEMU, and native SUD where supported)
	cd prototype && $(TOOLS_RUN) uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_backend_equiv_rs.py
	cd prototype && $(TOOLS_RUN) uv run python test_appliance_compat32.py

.PHONY: bench-backends
bench-backends: ## Compare live backend filesystem workloads (set SARUN_BENCH_ROUNDS=N)
	cd prototype && $(TOOLS_RUN) uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python benchmark_backends.py

.PHONY: test-backend-workloads
test-backend-workloads: ## Strict real-tool matrix across every locally runnable backend
	cd prototype && $(TOOLS_RUN) uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
	  python test_backend_workloads.py

.PHONY: validate-backends
validate-backends: ## Build appliances, run all backend gates, then print comparable benchmark medians
	$(MAKE) appliances
	$(MAKE) engine
	$(MAKE) test-backends
	$(MAKE) test-backend-workloads
	$(MAKE) bench-backends

.PHONY: validate-backends-kvm
validate-backends-kvm: ## Require KVM, then run the complete backend validation and benchmark gate
	@test -r /dev/kvm && test -w /dev/kvm || { \
	  echo "validate-backends-kvm needs read/write access to /dev/kvm" >&2; exit 1; }
	SARUN_REQUIRE_KVM=1 $(MAKE) validate-backends

# ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (engine/target/, ./sarun symlink, __pycache__)
	rm -rf engine/target
	rm -f sarun
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
