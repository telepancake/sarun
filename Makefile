# Makefile — the complete list of commands for this repo.
#
# Run `make` (no arguments) or `make help` to see every target with a
# one-line description. Each target below is documented with a `##`
# comment on its rule line — that is what `make help` prints, so keep
# the two in sync when you add a new target.
#
# Nothing here is magic: every recipe is a thin wrapper around the
# exact commands documented in CLAUDE.md and README.md. The Makefile
# exists so you don't have to remember (or copy-paste) them.
#
# Two binaries share the name "sarun":
#   * prototype/sarun   — the Python prototype that the Rust port was
#                         developed and tested against. uv shebang;
#                         first run also builds patched pyfuse3 (~25s,
#                         cached after). It lives under prototype/ so
#                         a top-level `./sarun` doesn't accidentally
#                         drop you into the slow first-run path.
#   * engine/.../sarun  — the Rust port (the production target). Same
#                         control protocol; a full standalone UI+engine.
#
# `make engine` drops a `./sarun` SYMLINK at the repo root pointing at the
# freshly-built static musl binary, so you can invoke it as `./sarun`
# without spelunking into engine/target/. `make run` execs that symlink
# when present, else the Python prototype. `make clean` removes the
# symlink. .gitignore'd.

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
run: ## Start sarun (./sarun symlink from the last engine build, else the Python prototype)
	@if [ -x ./sarun ]; then \
	  echo "→ ./sarun → $$(readlink ./sarun 2>/dev/null || echo ./sarun)"; \
	  exec ./sarun; \
	else \
	  echo "→ prototype/sarun  (Python prototype — first run builds patched pyfuse3, ~25s)"; \
	  echo "  (build the Rust port with 'make engine' to get a top-level ./sarun)"; \
	  exec prototype/sarun; \
	fi

.PHONY: run-py
run-py: ## Start the Python prototype specifically (prototype/sarun)
	prototype/sarun

.PHONY: warmup
warmup: ## Pre-build patched pyfuse3 + uv deps so prototype/sarun starts instantly
	prototype/sarun -h >/dev/null

.PHONY: sarun-help
sarun-help: ## Show the prototype's CLI help (prototype/sarun -h)
	prototype/sarun -h

# ---- System dependencies --------------------------------------------------
#
# The first prototype/sarun run builds a patched pyfuse3 and needs a C
# toolchain plus libfuse3 dev headers; boxes need bubblewrap. Everything
# Python is pulled by uv from prototype/sarun's PEP 723 header — do NOT
# pip install anything.

.PHONY: deps
deps: ## Install system packages (FUSE, bubblewrap, gcc; iproute2 + tshark for net tests)
	apt-get install -y libfuse3-dev fuse3 pkg-config bubblewrap gcc
	apt-get install -y iproute2 tshark   # only needed by test_net_rs.py

# ---- Rust port ------------------------------------------------------------
#
# Builds engine/target/x86_64-unknown-linux-musl/release/sarun, the
# fully-static Rust port — the project's only shipped binary. Built with
# cargo-zigbuild + ziglang so no system musl-tools is required. The cargo
# default target is set to musl in engine/.cargo/config.toml; tests and
# everything else go through this single target.

.PHONY: engine
engine: ## Build the Rust port (fully-static musl binary; cargo-zigbuild + zig)
	@command -v uv >/dev/null || { echo "engine needs uv (https://docs.astral.sh/uv/)"; exit 1; }
	uv tool install --with ziglang cargo-zigbuild
	rustup target add x86_64-unknown-linux-musl
	cd engine && PATH="$$(uv tool dir)/cargo-zigbuild/bin:$$HOME/.local/bin:$$PATH" \
	  cargo zigbuild --release --target x86_64-unknown-linux-musl
	@ln -sfn engine/target/x86_64-unknown-linux-musl/release/sarun sarun
	@echo "→ ./sarun → engine/target/x86_64-unknown-linux-musl/release/sarun"

# ---- Tests ----------------------------------------------------------------

.PHONY: test
test: ## Run the test suite (pytest-xdist; excludes test_e2e.py, test_pjdfstest.py, test_oci.py)
	cd prototype && uv run --with pytest --with pytest-xdist --with pytest-timeout \
	  --with "textual>=0.60" --with "wcmatch>=8.4" --with "pyfuse3>=3.2" \
	  --with "trio>=0.22" --with "python-magic>=0.4" \
	  pytest -q -p no:cacheprovider -n auto --dist=loadscope \
	  --timeout=180 --timeout-method=signal \
	  --ignore=test_e2e.py --ignore=test_pjdfstest.py --ignore=test_oci.py

.PHONY: test-e2e
test-e2e: ## Run the end-to-end tests (real UI + real sandboxes; minutes)
	prototype/test_e2e.py

.PHONY: test-oci
test-oci: ## Run the hermetic OCI tests (synthetic archive; real Rust engine; needs `make engine`)
	prototype/test_oci.py

# ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (engine/target/, ./sarun symlink, __pycache__)
	rm -rf engine/target
	rm -f sarun
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
