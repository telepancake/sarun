HOST_ARCH_RAW := $(shell uname -m)
HOST_ARCH := $(if $(filter arm64,$(HOST_ARCH_RAW)),aarch64,$(if $(filter amd64,$(HOST_ARCH_RAW)),x86_64,$(HOST_ARCH_RAW)))
STATIC_TARGET ?= $(HOST_ARCH)-unknown-linux-musl
CARGO ?= cargo

.DEFAULT_GOAL := build

.PHONY: vendor build release debug test
vendor:
	python3 tools/vendor.py

# The user-facing build is an optimized, fully static Linux executable. Zig is
# the linker/libc provider so this is reproducible from Linux and macOS hosts.
build: release

release: vendor
	$(CARGO) zigbuild --locked --release --target $(STATIC_TARGET)

debug: vendor
	$(CARGO) build --locked

test: vendor
	$(CARGO) test --locked
