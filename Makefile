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
run: ## Start the sarun UI/server (./sarun)
	./sarun

.PHONY: sarun-help
sarun-help: ## Show sarun's own CLI help (./sarun -h)
	./sarun -h

# ---- System dependencies --------------------------------------------------
#
# The first ./sarun run builds a patched pyfuse3 and needs a C toolchain
# plus libfuse3 dev headers; boxes need bubblewrap. Everything Python is
# pulled by uv from sarun's PEP 723 header — do NOT pip install anything.

.PHONY: deps
deps: ## Install system packages (libfuse3-dev, fuse3, pkg-config, bubblewrap)
	apt-get install -y libfuse3-dev fuse3 pkg-config bubblewrap

# ---- Rust engine ----------------------------------------------------------

.PHONY: engine
engine: ## Build the Rust engine (release, dynamic glibc — what tests use)
	cd engine && cargo build --release

.PHONY: engine-musl
engine-musl: ## Build the fully-static musl engine binary
	apt-get install -y musl-tools
	rustup target add x86_64-unknown-linux-musl
	cd engine && cargo build --release --target x86_64-unknown-linux-musl

# ---- Tests ----------------------------------------------------------------

.PHONY: test
test: ## Run the Python test suite (excludes sakar/* and pjdfstest)
	uv run --with pytest --with pytest-timeout --with "textual>=0.60" \
	  --with "wcmatch>=8.4" --with "pyfuse3>=3.2" \
	  --with "trio>=0.22" --with "python-magic>=0.4" \
	  pytest -q -p no:cacheprovider --ignore=test_e2e.py \
	  --ignore=test_sakar.py --ignore=test_sakar_e2e.py --ignore=test_pjdfstest.py

.PHONY: test-e2e
test-e2e: ## Run the end-to-end tests (real UI + real sandboxes; minutes)
	./test_e2e.py

# ---- Housekeeping ---------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts (engine target/, __pycache__)
	rm -rf engine/target
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
