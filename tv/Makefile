CC ?= cc
CXX ?= g++
MOD_DIR := mod
PWD     := $(shell pwd)
MOK_KEY ?= $(PWD)/MOK.priv
MOK_CER ?= $(PWD)/MOK.der

all: tv sudtrace upttrace sud-bins mod-bins

.PHONY: sud-bins mod-bins
sud-bins:
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
SUD_SRCS    := sud/wrapper.c sud/state.c sud/addin.c sud/raw.c sud/elf.c sud/handler.c sud/loader.c
SUD_SRCS    += libc-fs/libc.c libc-fs/deps/printf/printf.c
ifneq ($(filter sud/trace,$(SUD_ADDINS)),)
SUD_CFLAGS  += -DSUD_ADDIN_TRACE
SUD_SRCS    += sud/trace/event.c sud/trace/addin.c
endif
ifneq ($(filter sud/path_remap,$(SUD_ADDINS)),)
SUD_CFLAGS  += -DSUD_ADDIN_PATH_REMAP
SUD_SRCS    += sud/path_remap/addin.c
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
sudtrace: sud/sudtrace.c wire/wire.h trace/trace.h
	$(CC) $(CFLAGS) -o $@ sud/sudtrace.c

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

.PHONY: wire-test
wire-test: tv
	./tv dump --selftest

.PHONY: all keygen sign load unload clean clean-bins install test
test: tv libc-fs-test
	./tv test

clean-bins:
	rm -f tv sud32 sud64 mod/modtrace
	-$(MAKE) -C $(ZSTD_DIR) clean
