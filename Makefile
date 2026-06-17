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
# `make engine` and `make engine-musl` drop a `./sarun` SYMLINK at the
# repo root pointing at whichever build they just produced — so you can
# invoke the freshly-built binary as `./sarun` without spelunking into
# engine/target/. `make run` execs that symlink when present, else the
# Python prototype. `make clean` removes the symlink. .gitignore'd.

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
	  echo "  (build the Rust port with 'make engine' or 'make engine-musl' to get a top-level ./sarun)"; \
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
deps: ## Install system packages (libfuse3-dev, fuse3, pkg-config, bubblewrap)
	apt-get install -y libfuse3-dev fuse3 pkg-config bubblewrap

# ---- Rust port ------------------------------------------------------------
#
# Builds engine/target/.../sarun, the Rust port of the Python prototype.
# Same control protocol — a built binary is what `make run` picks up.

.PHONY: engine
engine: ## Build the Rust port (release, dynamic glibc — what `make run` prefers, what tests use)
	cd engine && cargo build --release
	@ln -sfn engine/target/release/sarun sarun
	@echo "→ ./sarun → engine/target/release/sarun"

.PHONY: engine-musl
engine-musl: ## Build the Rust port as a fully-static musl binary (cargo-zigbuild + zig)
	@command -v uv >/dev/null || { echo "engine-musl needs uv (https://docs.astral.sh/uv/)"; exit 1; }
	uv tool install --with ziglang cargo-zigbuild
	rustup target add x86_64-unknown-linux-musl
	mkdir -p engine/target/zigshim
	printf '#!/bin/sh\nexec cargo-zigbuild zig cc -- -target x86_64-linux-musl "$$@"\n' > engine/target/zigshim/musl-gcc
	chmod +x engine/target/zigshim/musl-gcc
	cd engine && PATH="$(CURDIR)/engine/target/zigshim:$$(uv tool dir)/cargo-zigbuild/bin:$$HOME/.local/bin:$$PATH" \
	  cargo zigbuild --release --target x86_64-unknown-linux-musl
	@ln -sfn engine/target/x86_64-unknown-linux-musl/release/sarun sarun
	@echo "→ ./sarun → engine/target/x86_64-unknown-linux-musl/release/sarun"

# ---- Tests ----------------------------------------------------------------

.PHONY: test
test: ## Run the Python test suite (in prototype/; excludes sakar/* and pjdfstest)
	cd prototype && uv run --with pytest --with pytest-timeout --with "textual>=0.60" \
	  --with "wcmatch>=8.4" --with "pyfuse3>=3.2" \
	  --with "trio>=0.22" --with "python-magic>=0.4" \
	  pytest -q -p no:cacheprovider --ignore=test_e2e.py \
	  --ignore=test_pjdfstest.py

.PHONY: test-e2e
test-e2e: ## Run the end-to-end tests (real UI + real sandboxes; minutes)
	prototype/test_e2e.py

# ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (engine/target/, ./sarun symlink, __pycache__)
	rm -rf engine/target
	rm -f sarun
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
