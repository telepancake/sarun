CC ?= cc
CXX ?= g++
MOD_DIR := mod
PWD     := $(shell pwd)
MOK_KEY ?= $(PWD)/MOK.priv
MOK_CER ?= $(PWD)/MOK.der

all: tv upttrace sud-bins mod-bins

.PHONY: sud-bins mod-bins
sud-bins: fs-ring-test
	$(MAKE) sud64 sud32

mod-bins:
	$(MAKE) -C $(MOD_DIR) all

# Generate a MOK keypair (one-time).  After this, run:
#   sudo mokutil --import MOK.der
# then reboot and enroll through the MOK manager.
keygen:
	openssl req -new -x509 -newkey rsa:2048 \
	        -keyout $(MOK_KEY) -outform DER -out $(MOK_CER) \
	        -nodes -days 36500 -subj "/CN=Proctrace Module Signing Key/"
	@echo ""
	@echo "Key generated.  Now run:"
	@echo "  sudo mokutil --import $(MOK_CER)"
	@echo "Then reboot and enroll the key in the MOK manager."

# Sign the module (requires MOK.priv + MOK.der from keygen).
sign: all
	$(MAKE) -C $(MOD_DIR) sign

# Build + sign + load in one step.
load: sign
	$(MAKE) -C $(MOD_DIR) load

unload:
	$(MAKE) -C $(MOD_DIR) unload

clean:
	$(MAKE) -C $(MOD_DIR) clean
	$(MAKE) -C libc-fs clean
	rm -f tv upttrace sud32 sud64
	rm -f build/fs_ring_abi32.o build/fs_ring_abi64.o
	rm -f build/fs_ring_client32.o build/fs_ring_client64.o
	rm -f build/fs_fd_lane32.o build/fs_fd_lane64.o
	rm -f build/fs_fuse_client32.o build/fs_fuse_client64.o
	rm -f build/fs_vfs_client32.o build/fs_vfs_client64.o
	rm -f build/fs_client_test32 build/fs_client_test64
	rm -f build/fs_fd_lane_test32 build/fs_fd_lane_test64
	rm -f build/fs_fuse_client_test32 build/fs_fuse_client_test64
	rm -f build/fs_vfs_test32 build/fs_vfs_test64

install:
	$(MAKE) -C $(MOD_DIR) install

ZSTD_DIR  := deps/zstd/lib
ZSTD_LIB  := $(ZSTD_DIR)/libzstd.a

DUCKDB_DIR := deps/duckdb
DUCKDB_AMAL_DIR := $(DUCKDB_DIR)/src/amalgamation
DUCKDB_HPP := $(DUCKDB_AMAL_DIR)/duckdb.hpp
DUCKDB_CPP := $(DUCKDB_AMAL_DIR)/duckdb.cpp
DUCKDB_INC := $(DUCKDB_DIR)/src/include
DUCKDB_OBJ := build/duckdb.o

CXXFLAGS := -std=c++23 -O2 -I. -I$(ZSTD_DIR) -I$(DUCKDB_INC)
CFLAGS   := -std=c11   -O2 -I. -I$(ZSTD_DIR)
TV_LIBS := -lm -pthread -ldl $(ZSTD_LIB)
# Static link works fine even though duckdb pulls in dlopen/getaddrinfo
# (those code paths exist for extensions / network reads we don't use).
TV_LDFLAGS := -static
SUD_CFLAGS  := -O2 -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 -ffreestanding -fno-builtin -fno-stack-protector -fno-pie -fomit-frame-pointer -I.
SUD_LDFLAGS := -nostdlib -static -no-pie -Wl,--build-id=none

# The sud wrappers build with the SAME zig toolchain as the engine —
# no host C compiler (a non-x86 or gcc-less host must still produce
# the x86 wrappers). zig comes from the ziglang wheel that
# `make engine` installs alongside cargo-zigbuild (`uv tool install
# --with ziglang cargo-zigbuild`); override ZIG to use another zig.
# -ffreestanding makes zig cc drop its bundled include dirs, so the
# musl + kernel-UAPI headers come back via -idirafter (host /usr/
# include is never consulted). lld has no -Ttext-segment; the image
# base flag is --image-base (same placement, checked by the
# __sud_begin/__sud_end bracketing). No libgcc: zig cannot attach
# compiler_rt to a -nostdlib link, so the 32-bit 64-bit-division
# intrinsics are in-tree (libc-fs/divdi3.c).
UV_TOOL_DIR := $(shell uv tool dir 2>/dev/null)
ZIG ?= $(firstword $(wildcard $(UV_TOOL_DIR)/cargo-zigbuild/lib/python3*/site-packages/ziglang/zig) $(shell command -v zig 2>/dev/null))
ifeq ($(ZIG),)
$(warning zig not found: run `uv tool install --with ziglang cargo-zigbuild` or set ZIG=/path/to/zig)
endif
ZIG_LIBC_INC := $(patsubst %/zig,%,$(ZIG))/lib/libc/include
SUD_INC64 := -idirafter $(ZIG_LIBC_INC)/x86_64-linux-musl -idirafter $(ZIG_LIBC_INC)/generic-musl -idirafter $(ZIG_LIBC_INC)/x86-linux-any -idirafter $(ZIG_LIBC_INC)/any-linux-any
SUD_INC32 := -idirafter $(ZIG_LIBC_INC)/x86-linux-musl -idirafter $(ZIG_LIBC_INC)/generic-musl -idirafter $(ZIG_LIBC_INC)/x86-linux-any -idirafter $(ZIG_LIBC_INC)/any-linux-any
SUD_CC64 := $(ZIG) cc -target x86_64-linux-musl $(SUD_INC64)
# -mstackrealign: clang emits 16-byte-aligned SSE stores for big stack
# locals and ASSUMES aligned entry; the freestanding 32-bit _start
# inherits whatever alignment the kernel's argv/envp sizes produce, so
# every function realigns its own frame (a GP-fault segfault that
# comes and goes with the environment size otherwise).
SUD_CC32 := $(ZIG) cc -target x86-linux-musl -mstackrealign $(SUD_INC32)
SUD_BASE64 := -Wl,--image-base=0x40000000
SUD_BASE32 := -Wl,--image-base=0x20000000

# Set SIGSYS_DIAG=1 to enable the SIGSYS handler entry/exit diagnostic dumps.
# This prints the saved ucontext (registers, stack pointer, uc_stack) at both
# handler entry and exit so that signal-frame corruption can be detected.
# Example:  make SIGSYS_DIAG=1 sud32
SIGSYS_DIAG ?= 0
ifeq ($(SIGSYS_DIAG),1)
SUD_CFLAGS  += -DSUDTRACE_SIGSYS_DIAG
endif
SUD_SRCS    := sud/wrapper.c sud/state.c sud/addin.c sud/raw.c sud/elf.c sud/handler.c sud/loader.c sud/runtime_config.c
SUD_SRCS    += libc-fs/libc.c libc-fs/deps/printf/printf.c libc-fs/divdi3.c
SUD_HDRS    := $(shell find sud libc-fs -name '*.h' 2>/dev/null)
SUD_CFLAGS  += -DSUD_ADDIN_TRACE -DSUD_ADDIN_FS
SUD_SRCS    += sud/trace/event.c sud/trace/addin.c
SUD_SRCS    += sud/fs/addin.c sud/fs/vfs.c sud/fs/fuse_client.c sud/fs/client.c sud/fs/fd_lane.c
SUD_NATIVE  := $(if $(filter x86_64,$(shell uname -m)),sud64,sud32)

$(ZSTD_LIB):
	$(MAKE) -C $(ZSTD_DIR) libzstd.a

# DuckDB amalgamation is huge. Default to clang -O0 because it's
# noticeably faster (~2-3 min vs 7+ min with g++ -O2) and uses ~5 GB
# RAM vs ~14 GB. The resulting .o is ~150 MB but only re-linking is
# needed when you change tv sources, not the duckdb source. For release
# builds, override:  make DUCKDB_CXX=g++ DUCKDB_OPT=-O2
DUCKDB_CXX ?= clang++
DUCKDB_OPT ?= -O0

# DuckDB is vendored as a single ~25 MB amalgamation file. We build it
# ourselves through tools/duckdb_amalgamate.py instead of running the
# upstream scripts/amalgamation.py directly because the upstream default
# bakes in the *dummy* extension loader — which silently strips out the
# core_functions extension where SUM, AVG, COUNT(DISTINCT), bitwise &,
# regexp_*, FIRST(... ORDER BY ...) and ~hundreds of other operators
# actually live. Our wrapper relinks the real loader and concatenates
# extension/core_functions into the amalgamation.
$(DUCKDB_CPP): tools/duckdb_amalgamate.py | $(DUCKDB_DIR)/scripts/amalgamation.py
	python3 tools/duckdb_amalgamate.py

$(DUCKDB_HPP): $(DUCKDB_CPP)
	@true

$(DUCKDB_OBJ): $(DUCKDB_CPP)
	@mkdir -p build
	$(DUCKDB_CXX) -std=c++17 $(DUCKDB_OPT) -I$(DUCKDB_INC) -c $(DUCKDB_CPP) -o $@

# Single statically-linked tv binary with subcommands. Folds in what
# used to be separate wiredump/fv binaries — see main.cpp's subcommand
# dispatch (dump, fv, ingest, test). Tracers (upttrace,
# modtrace) are now separate binaries; pick one via `tv --tracer EXE`.
TV_CXX_SRCS := main.cpp engine.cpp tests.cpp \
               trace/trace_stream.cpp \
               tv_db.cpp data_source.cpp fv.cpp
TV_C_SRCS   := tools/wiredump/wiredump.c
TV_C_OBJS   := $(patsubst %.c,build/%.o,$(TV_C_SRCS))
TV_HDRS := engine.h trace/trace_stream.h tv_db.h data_source.h \
           wire/wire.h trace/trace.h $(DUCKDB_HPP)

build/%.o: %.c
	@mkdir -p $(dir $@)
	$(CC) $(CFLAGS) -c $< -o $@

tv: $(TV_CXX_SRCS) $(TV_C_OBJS) $(TV_HDRS) $(ZSTD_LIB) $(DUCKDB_OBJ)
	$(CXX) $(CXXFLAGS) $(TV_LDFLAGS) -o tv $(TV_CXX_SRCS) $(TV_C_OBJS) \
	    $(DUCKDB_OBJ) $(TV_LIBS)

# Standalone upttrace (ptrace-based userspace tracer, works anywhere).
UPTTRACE_HDRS := wire/wire.h trace/trace.h
upttrace: upt/upttrace.cpp $(UPTTRACE_HDRS) $(ZSTD_LIB)
	$(CXX) $(CXXFLAGS) -o $@ upt/upttrace.cpp -pthread $(ZSTD_LIB)

# Release build: production-grade flags, asserts off, debug info stripped.
# Use:  make release        (drop the dev-friendly default `tv` binary first)
# Note: `tv` and `tv-release` share intermediate objects with the dev build
# only via $(DUCKDB_OBJ). If you want a pristine release rebuild from the
# DuckDB amalgamation too, also pass DUCKDB_OPT=-O2 explicitly:
#   make clean-tv && make release DUCKDB_OPT=-O2 DUCKDB_CXX=g++
.PHONY: release clean-tv
release: CXXFLAGS := -std=c++23 -O2 -DNDEBUG -fno-stack-protector -I. -I$(ZSTD_DIR) -I$(DUCKDB_INC)
release: CFLAGS   := -std=c11   -O2 -DNDEBUG -I. -I$(ZSTD_DIR)
release: TV_LDFLAGS := -static -s
release: tv upttrace

clean-tv:
	rm -f tv $(TV_C_OBJS)

sud64: $(SUD_SRCS) $(SUD_HDRS) sudtrace.lds
	$(SUD_CC64) $(SUD_CFLAGS) $(SUD_LDFLAGS) $(SUD_BASE64) -T sudtrace.lds -o sud64 $(SUD_SRCS)

sud32: $(SUD_SRCS) $(SUD_HDRS) sudtrace.lds
	$(SUD_CC32) $(SUD_CFLAGS) $(SUD_LDFLAGS) $(SUD_BASE32) -T sudtrace.lds -o sud32 $(SUD_SRCS)

.PHONY: fs-ring-test
fs-ring-test: build/fs_ring_abi64.o build/fs_ring_abi32.o \
              build/fs_ring_client64.o build/fs_ring_client32.o \
              build/fs_fd_lane64.o build/fs_fd_lane32.o \
              build/fs_fuse_client64.o build/fs_fuse_client32.o \
              build/fs_vfs_client64.o build/fs_vfs_client32.o \
              build/fs_client_test64 build/fs_client_test32 \
              build/fs_fd_lane_test64 build/fs_fd_lane_test32 \
              build/fs_fuse_client_test64 build/fs_fuse_client_test32 \
              build/fs_vfs_test64 build/fs_vfs_test32
	./build/fs_client_test64
	./build/fs_client_test32
	./build/fs_fd_lane_test64
	./build/fs_fd_lane_test32
	./build/fs_fuse_client_test64
	./build/fs_fuse_client_test32
	./build/fs_vfs_test64
	./build/fs_vfs_test32

build/fs_ring_abi64.o: sud/fs/tests/ring_abi.c sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) -c -o $@ $<

build/fs_ring_abi32.o: sud/fs/tests/ring_abi.c sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) -c -o $@ $<

build/fs_ring_client64.o: sud/fs/client.c sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) -c -o $@ $<

build/fs_ring_client32.o: sud/fs/client.c sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) -c -o $@ $<

build/fs_fd_lane64.o: sud/fs/fd_lane.c sud/fs/fd_lane.h sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) -c -o $@ $<

build/fs_fd_lane32.o: sud/fs/fd_lane.c sud/fs/fd_lane.h sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) -c -o $@ $<

build/fs_fuse_client64.o: sud/fs/fuse_client.c sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) -c -o $@ $<

build/fs_fuse_client32.o: sud/fs/fuse_client.c sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) -c -o $@ $<

build/fs_vfs_client64.o: sud/fs/vfs.c sud/fs/vfs.h sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) -c -o $@ $<

build/fs_vfs_client32.o: sud/fs/vfs.c sud/fs/vfs.h sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) -c -o $@ $<

FS_CLIENT_TEST_SRCS := sud/fs/tests/test_client.c sud/fs/client.c \
                       libc-fs/libc.c libc-fs/deps/printf/printf.c \
                       libc-fs/divdi3.c

build/fs_client_test64: $(FS_CLIENT_TEST_SRCS) sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_CLIENT_TEST_SRCS)

build/fs_client_test32: $(FS_CLIENT_TEST_SRCS) sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_CLIENT_TEST_SRCS)

FS_FD_LANE_TEST_SRCS := sud/fs/tests/test_fd_lane.c sud/fs/fd_lane.c \
                        sud/fs/client.c libc-fs/libc.c \
                        libc-fs/deps/printf/printf.c libc-fs/divdi3.c

build/fs_fd_lane_test64: $(FS_FD_LANE_TEST_SRCS) sud/fs/fd_lane.h sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_FD_LANE_TEST_SRCS)

build/fs_fd_lane_test32: $(FS_FD_LANE_TEST_SRCS) sud/fs/fd_lane.h sud/fs/client.h sud/fs/ring.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_FD_LANE_TEST_SRCS)

FS_FUSE_CLIENT_TEST_SRCS := sud/fs/tests/test_fuse_client.c \
                            sud/fs/fuse_client.c sud/fs/client.c \
                            libc-fs/libc.c libc-fs/deps/printf/printf.c \
                            libc-fs/divdi3.c

build/fs_fuse_client_test64: $(FS_FUSE_CLIENT_TEST_SRCS) sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_FUSE_CLIENT_TEST_SRCS)

build/fs_fuse_client_test32: $(FS_FUSE_CLIENT_TEST_SRCS) sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_FUSE_CLIENT_TEST_SRCS)

FS_VFS_TEST_SRCS := sud/fs/tests/test_vfs.c sud/fs/addin.c sud/fs/vfs.c \
                    sud/fs/fuse_client.c sud/fs/client.c sud/fs/fd_lane.c \
                    sud/runtime_config.c \
                    libc-fs/libc.c libc-fs/deps/printf/printf.c \
                    libc-fs/divdi3.c

build/fs_vfs_test64: $(FS_VFS_TEST_SRCS) sud/fs/vfs.h sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC64) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_VFS_TEST_SRCS)

build/fs_vfs_test32: $(FS_VFS_TEST_SRCS) sud/fs/vfs.h sud/fs/fuse_client.h sud/fs/client.h
	@mkdir -p build
	$(SUD_CC32) $(SUD_CFLAGS) $(SUD_LDFLAGS) -o $@ $(FS_VFS_TEST_SRCS)

.PHONY: libc-fs-test
libc-fs-test:
	$(MAKE) -C libc-fs test

.PHONY: wire-test
wire-test: tv
	./tv dump --selftest

.PHONY: all keygen sign load unload clean clean-bins install test
test: tv libc-fs-test
	./tv test

clean-bins:
	rm -f tv sud32 sud64 mod/modtrace
	-$(MAKE) -C $(ZSTD_DIR) clean
