CC ?= cc
CXX ?= g++
MOD_DIR := mod
PWD     := $(shell pwd)
MOK_KEY ?= $(PWD)/MOK.priv
MOK_CER ?= $(PWD)/MOK.der

all: tv sudtrace upttrace sud-bins mod-bins

.PHONY: sud-bins mod-bins
sud-bins: path-remap-test dispatcher-test inramfs-test fake-exec-test
	$(MAKE) $(SUD_NATIVE)

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
	rm -f tv sudtrace upttrace sud32 sud64
	rm -f build/path_remap_test32 build/path_remap_test64
	rm -f build/dispatcher_test_both32 build/dispatcher_test_both64
	rm -f build/dispatcher_test_pathremap32 build/dispatcher_test_pathremap64
	rm -f build/dispatcher_test_trace32 build/dispatcher_test_trace64
	rm -f build/inramfs_test32 build/inramfs_test64
	rm -f build/fake_exec_test32 build/fake_exec_test64

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

# Set SIGSYS_DIAG=1 to enable the SIGSYS handler entry/exit diagnostic dumps.
# This prints the saved ucontext (registers, stack pointer, uc_stack) at both
# handler entry and exit so that signal-frame corruption can be detected.
# Example:  make SIGSYS_DIAG=1 sud32
SIGSYS_DIAG ?= 0
ifeq ($(SIGSYS_DIAG),1)
SUD_CFLAGS  += -DSUDTRACE_SIGSYS_DIAG
endif
SUD_ADDINS ?= sud/trace
SUD_SRCS    := sud/wrapper.c sud/state.c sud/addin.c sud/raw.c sud/elf.c sud/handler.c sud/loader.c sud/runtime_config.c
SUD_SRCS    += libc-fs/libc.c libc-fs/deps/printf/printf.c
ifneq ($(filter sud/trace,$(SUD_ADDINS)),)
SUD_CFLAGS  += -DSUD_ADDIN_TRACE
SUD_SRCS    += sud/trace/event.c sud/trace/addin.c
endif
ifneq ($(filter sud/path_remap,$(SUD_ADDINS)),)
SUD_CFLAGS  += -DSUD_ADDIN_PATH_REMAP
SUD_SRCS    += sud/path_remap/addin.c sud/path_remap/overlay.c sud/path_remap/path.c sud/path_remap/fakeroot.c
endif
ifneq ($(filter sud/fake-exec,$(SUD_ADDINS)),)
SUD_CFLAGS  += -DSUD_ADDIN_FAKE_EXEC
SUD_SRCS    += sud/fake-exec/addin.c sud/fake-exec/builtins.c sud/fake-exec/detect.c
endif
ifneq ($(filter sud/inramfs,$(SUD_ADDINS)),)
SUD_CFLAGS  += -DSUD_ADDIN_INRAMFS
SUD_SRCS    += sud/inramfs/addin.c sud/inramfs/super.c sud/inramfs/vfs.c
# inramfs_glue.c lives under path_remap/ but is the path-bearing
# dispatch table for inramfs.  It only makes sense when both addins
# are configured (path_remap drives, inramfs serves).
ifneq ($(filter sud/path_remap,$(SUD_ADDINS)),)
SUD_SRCS    += sud/path_remap/inramfs_glue.c
endif
endif
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
# dispatch (dump, fv, ingest, test). Tracers (upttrace, sudtrace,
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

# Standalone sudtrace launcher (the tracer binary itself; calls into
# sud32/sud64 wrappers to drive the traced process). See sud/sudtrace.c
# for the design notes.
sudtrace: sud/sudtrace.c sud/runtime_config.c sud/runtime_config.h wire/wire.h trace/trace.h
	$(CC) $(CFLAGS) -o $@ sud/sudtrace.c sud/runtime_config.c

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
release: tv sudtrace upttrace

clean-tv:
	rm -f tv $(TV_C_OBJS)

sud64: $(SUD_SRCS) sudtrace.lds
	$(CC) -m64 $(SUD_CFLAGS) $(SUD_LDFLAGS) -Wl,-Ttext-segment=0x40000000 -T sudtrace.lds -o sud64 $(SUD_SRCS) -lgcc

sud32: $(SUD_SRCS) sudtrace.lds
	$(CC) -m32 $(SUD_CFLAGS) $(SUD_LDFLAGS) -Wl,-Ttext-segment=0x20000000 -T sudtrace.lds -o sud32 $(SUD_SRCS) -lgcc

.PHONY: libc-fs-test
libc-fs-test:
	$(MAKE) -C libc-fs test

# path_remap overlay self-tests.  Built freestanding for both 32-bit
# and 64-bit just like the sud wrapper itself, so any architecture-
# specific bug in path resolution / stat layout / syscall numbering
# is caught in CI before the wrapper goes near a traced program.
PATH_REMAP_TEST_SRCS := sud/path_remap/tests/test_overlay.c \
                        sud/path_remap/tests/test_fakeroot.c \
                        sud/path_remap/overlay.c \
                        sud/path_remap/path.c \
                        sud/path_remap/fakeroot.c \
                        sud/runtime_config.c \
                        libc-fs/libc.c libc-fs/deps/printf/printf.c
PATH_REMAP_TEST_HDRS := sud/path_remap/overlay.h \
                        sud/path_remap/path.h \
                        sud/path_remap/fakeroot.h \
                        sud/runtime_config.h \
                        libc-fs/libc.h libc-fs/fmt.h
.PHONY: path-remap-test
path-remap-test: build/path_remap_test64 build/path_remap_test32
	@echo '--- running 64-bit path_remap overlay tests ---'
	./build/path_remap_test64
	@echo '--- running 32-bit path_remap overlay tests ---'
	./build/path_remap_test32

build/path_remap_test64: $(PATH_REMAP_TEST_SRCS) $(PATH_REMAP_TEST_HDRS)
	@mkdir -p build
	$(CC) -m64 $(SUD_CFLAGS) $(SUD_LDFLAGS) \
	    -o $@ $(PATH_REMAP_TEST_SRCS) -lgcc

build/path_remap_test32: $(PATH_REMAP_TEST_SRCS) $(PATH_REMAP_TEST_HDRS)
	@mkdir -p build
	$(CC) -m32 $(SUD_CFLAGS) $(SUD_LDFLAGS) \
	    -o $@ $(PATH_REMAP_TEST_SRCS) -lgcc

# path_remap × trace dispatch-order tests.  Built three times in each
# bitness — both addins, path_remap-only, trace-only — to verify the
# dispatcher contract: trace runs first and observes the program-
# supplied args, path_remap runs second and rewrites them for the
# kernel.  Either addin must work on its own.
DISPATCH_TEST_BASE_CFLAGS := -O2 -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
                             -ffreestanding -fno-builtin -fno-stack-protector \
                             -fno-pie -fomit-frame-pointer -I.
DISPATCH_TEST_LDFLAGS     := -nostdlib -static -no-pie -Wl,--build-id=none
DISPATCH_TEST_COMMON_SRCS := sud/path_remap/tests/test_dispatcher.c \
                             sud/addin.c \
                             sud/runtime_config.c \
                             libc-fs/libc.c libc-fs/deps/printf/printf.c
DISPATCH_TEST_PATHREMAP_SRCS := sud/path_remap/addin.c sud/path_remap/overlay.c sud/path_remap/path.c sud/path_remap/fakeroot.c
DISPATCH_TEST_HDRS := sud/addin.h sud/path_remap/overlay.h sud/path_remap/path.h \
                      sud/path_remap/fakeroot.h \
                      sud/runtime_config.h \
                      libc-fs/libc.h libc-fs/fmt.h

DISPATCH_TEST_BIN64_BOTH := build/dispatcher_test_both64
DISPATCH_TEST_BIN32_BOTH := build/dispatcher_test_both32
DISPATCH_TEST_BIN64_PR   := build/dispatcher_test_pathremap64
DISPATCH_TEST_BIN32_PR   := build/dispatcher_test_pathremap32
DISPATCH_TEST_BIN64_TR   := build/dispatcher_test_trace64
DISPATCH_TEST_BIN32_TR   := build/dispatcher_test_trace32

.PHONY: dispatcher-test
dispatcher-test: $(DISPATCH_TEST_BIN64_BOTH) $(DISPATCH_TEST_BIN32_BOTH) \
                 $(DISPATCH_TEST_BIN64_PR)   $(DISPATCH_TEST_BIN32_PR) \
                 $(DISPATCH_TEST_BIN64_TR)   $(DISPATCH_TEST_BIN32_TR)
	@echo '--- 64-bit dispatcher tests (both addins) ---'
	./$(DISPATCH_TEST_BIN64_BOTH)
	@echo '--- 32-bit dispatcher tests (both addins) ---'
	./$(DISPATCH_TEST_BIN32_BOTH)
	@echo '--- 64-bit dispatcher tests (path_remap only) ---'
	./$(DISPATCH_TEST_BIN64_PR)
	@echo '--- 32-bit dispatcher tests (path_remap only) ---'
	./$(DISPATCH_TEST_BIN32_PR)
	@echo '--- 64-bit dispatcher tests (trace only) ---'
	./$(DISPATCH_TEST_BIN64_TR)
	@echo '--- 32-bit dispatcher tests (trace only) ---'
	./$(DISPATCH_TEST_BIN32_TR)

$(DISPATCH_TEST_BIN64_BOTH): $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) $(DISPATCH_TEST_HDRS)
	@mkdir -p build
	$(CC) -m64 $(DISPATCH_TEST_BASE_CFLAGS) -DSUD_ADDIN_TRACE -DSUD_ADDIN_PATH_REMAP \
	    $(DISPATCH_TEST_LDFLAGS) -o $@ \
	    $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) -lgcc

$(DISPATCH_TEST_BIN32_BOTH): $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) $(DISPATCH_TEST_HDRS)
	@mkdir -p build
	$(CC) -m32 $(DISPATCH_TEST_BASE_CFLAGS) -DSUD_ADDIN_TRACE -DSUD_ADDIN_PATH_REMAP \
	    $(DISPATCH_TEST_LDFLAGS) -o $@ \
	    $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) -lgcc

$(DISPATCH_TEST_BIN64_PR): $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) $(DISPATCH_TEST_HDRS)
	@mkdir -p build
	$(CC) -m64 $(DISPATCH_TEST_BASE_CFLAGS) -DSUD_ADDIN_PATH_REMAP \
	    $(DISPATCH_TEST_LDFLAGS) -o $@ \
	    $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) -lgcc

$(DISPATCH_TEST_BIN32_PR): $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) $(DISPATCH_TEST_HDRS)
	@mkdir -p build
	$(CC) -m32 $(DISPATCH_TEST_BASE_CFLAGS) -DSUD_ADDIN_PATH_REMAP \
	    $(DISPATCH_TEST_LDFLAGS) -o $@ \
	    $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_PATHREMAP_SRCS) -lgcc

$(DISPATCH_TEST_BIN64_TR): $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_HDRS)
	@mkdir -p build
	$(CC) -m64 $(DISPATCH_TEST_BASE_CFLAGS) -DSUD_ADDIN_TRACE \
	    $(DISPATCH_TEST_LDFLAGS) -o $@ \
	    $(DISPATCH_TEST_COMMON_SRCS) -lgcc

$(DISPATCH_TEST_BIN32_TR): $(DISPATCH_TEST_COMMON_SRCS) $(DISPATCH_TEST_HDRS)
	@mkdir -p build
	$(CC) -m32 $(DISPATCH_TEST_BASE_CFLAGS) -DSUD_ADDIN_TRACE \
	    $(DISPATCH_TEST_LDFLAGS) -o $@ \
	    $(DISPATCH_TEST_COMMON_SRCS) -lgcc

.PHONY: wire-test
wire-test: tv
	./tv dump --selftest

# inramfs add-in self-tests.  Built freestanding (-nostdlib) for both
# 32-bit and 64-bit so any architecture-specific bug in layout, alloc,
# locking, or the dispatch front-end is caught in CI.  The tests use
# /dev/shm as the backing region; CI must allow writes there.
INRAMFS_TEST_SRCS := sud/inramfs/tests/test_inramfs.c \
                     sud/inramfs/super.c sud/inramfs/vfs.c sud/inramfs/addin.c \
                     sud/path_remap/path.c \
                     sud/runtime_config.c \
                     libc-fs/libc.c libc-fs/deps/printf/printf.c
INRAMFS_TEST_HDRS := sud/inramfs/inramfs.h sud/inramfs/internal.h \
                     sud/path_remap/path.h \
                     sud/addin.h sud/runtime_config.h \
                     libc-fs/libc.h libc-fs/fmt.h
.PHONY: inramfs-test inramfs-test-e2e inramfs-test-sqlite
inramfs-test: build/inramfs_test64 build/inramfs_test32
	@echo '--- running 64-bit inramfs tests ---'
	./build/inramfs_test64
	@echo '--- running 32-bit inramfs tests ---'
	./build/inramfs_test32
	@echo '--- running inramfs end-to-end harness ---'
	$(MAKE) inramfs-test-e2e
	@echo '--- running inramfs sqlite end-to-end ---'
	$(MAKE) inramfs-test-sqlite

# inramfs end-to-end harness: runs sudtrace + strace over a tiny
# shell workload and asserts zero kernel file syscalls touch any
# path under the inramfs mount.  This is the "milestone 1" gate from
# the inramfs plan — without it nothing else is verifiable.
#
# The harness needs both the wrapper (sud64) and the launcher
# (sudtrace), built with the inramfs add-in compiled in.  We force
# SUD_ADDINS to include the add-in so a developer running this
# target directly (not via `make all`) gets a working build.
inramfs-test-e2e: tests/inramfs_e2e.sh
	@SUD_ADDINS="sud/trace sud/path_remap sud/inramfs" \
	    $(MAKE) -s sud64 sudtrace
	./tests/inramfs_e2e.sh

# Ultimate end-to-end: clone sqlite source, build sqlite3 from
# the amalgamation, then run two separate sqlite3 invocations
# against an inramfs-resident db (CREATE+INSERT, then SELECT
# validation).  Skipped automatically when the network is
# unreachable (e.g. air-gapped CI).
inramfs-test-sqlite: tests/inramfs_sqlite_e2e.sh
	@SUD_ADDINS="sud/trace sud/path_remap sud/inramfs" \
	    $(MAKE) -s sud64 sudtrace
	./tests/inramfs_sqlite_e2e.sh

build/inramfs_test64: $(INRAMFS_TEST_SRCS) $(INRAMFS_TEST_HDRS)
	@mkdir -p build
	$(CC) -m64 $(SUD_CFLAGS) -DSUD_ADDIN_INRAMFS $(SUD_LDFLAGS) \
	    -o $@ $(INRAMFS_TEST_SRCS) -lgcc

build/inramfs_test32: $(INRAMFS_TEST_SRCS) $(INRAMFS_TEST_HDRS)
	@mkdir -p build
	$(CC) -m32 $(SUD_CFLAGS) -DSUD_ADDIN_INRAMFS $(SUD_LDFLAGS) \
	    -o $@ $(INRAMFS_TEST_SRCS) -lgcc

# fake-exec addin self-tests.  Pure-function tests over the classifier
# and the builtin registry; the SYS_exit-emitting addin path is exercised
# end-to-end by tests/sudtrace_test.sh, not here.
FAKE_EXEC_TEST_SRCS := sud/fake-exec/tests/test_fake_exec.c \
                       sud/fake-exec/detect.c \
                       sud/fake-exec/builtins.c \
                       sud/runtime_config.c \
                       libc-fs/libc.c libc-fs/deps/printf/printf.c
FAKE_EXEC_TEST_HDRS := sud/fake-exec/fake_exec.h sud/fake-exec/builtins.h \
                       sud/addin.h sud/runtime_config.h \
                       libc-fs/libc.h libc-fs/fmt.h
.PHONY: fake-exec-test
fake-exec-test: build/fake_exec_test64
	@echo '--- running 64-bit fake-exec tests ---'
	./build/fake_exec_test64

build/fake_exec_test64: $(FAKE_EXEC_TEST_SRCS) $(FAKE_EXEC_TEST_HDRS)
	@mkdir -p build
	$(CC) -m64 $(SUD_CFLAGS) -DSUD_ADDIN_FAKE_EXEC $(SUD_LDFLAGS) \
	    -o $@ $(FAKE_EXEC_TEST_SRCS) -lgcc

.PHONY: all keygen sign load unload clean clean-bins install test
test: tv libc-fs-test
	./tv test

clean-bins:
	rm -f tv sud32 sud64 mod/modtrace
	-$(MAKE) -C $(ZSTD_DIR) clean
