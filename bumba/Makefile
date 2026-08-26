HOST_ARCH_RAW := $(shell uname -m)
HOST_ARCH := $(if $(filter arm64,$(HOST_ARCH_RAW)),aarch64,$(if $(filter amd64,$(HOST_ARCH_RAW)),x86_64,$(HOST_ARCH_RAW)))
STATIC_TARGET ?= $(HOST_ARCH)-unknown-linux-musl
RUST_TOOLCHAIN ?= 1.96.0
RUSTUP := $(shell command -v rustup 2>/dev/null)
CARGO ?= $(if $(RUSTUP),$(RUSTUP) run $(RUST_TOOLCHAIN) cargo,cargo)
RUSTC ?= $(if $(RUSTUP),$(shell $(RUSTUP) which --toolchain $(RUST_TOOLCHAIN) rustc),rustc)
RUSTDOC ?= $(if $(RUSTUP),$(shell $(RUSTUP) which --toolchain $(RUST_TOOLCHAIN) rustdoc),rustdoc)

.DEFAULT_GOAL := build

.PHONY: vendor toolchain static-toolchain build release debug test
vendor:
	python3 tools/vendor.py

toolchain:
	$(if $(RUSTUP),@if ! $(RUSTUP) run $(RUST_TOOLCHAIN) rustc --version >/dev/null 2>&1; then $(RUSTUP) toolchain install --profile minimal $(RUST_TOOLCHAIN); fi,@true)

static-toolchain: toolchain
	$(if $(RUSTUP),$(RUSTUP) target add --toolchain $(RUST_TOOLCHAIN) $(STATIC_TARGET),@true)

# The user-facing build is an optimized, fully static Linux executable. Zig is
# the linker/libc provider so this is reproducible from Linux and macOS hosts.
build: release

release: vendor static-toolchain
	RUSTC=$(RUSTC) RUSTDOC=$(RUSTDOC) $(CARGO) zigbuild --locked --release --target $(STATIC_TARGET)

debug: vendor toolchain
	RUSTC=$(RUSTC) RUSTDOC=$(RUSTDOC) $(CARGO) build --locked

test: vendor toolchain
	RUSTC=$(RUSTC) RUSTDOC=$(RUSTDOC) $(CARGO) test --locked
