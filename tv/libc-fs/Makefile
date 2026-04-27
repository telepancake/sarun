CC ?= cc
AR ?= ar

CFLAGS ?= -O2
LIBC_FS_CFLAGS := $(CFLAGS) -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
    -ffreestanding -fno-builtin -fno-stack-protector -fno-pie \
    -fomit-frame-pointer -I..

SRCS := libc.c deps/printf/printf.c
OBJS64 := build/libc.o build/printf.o
OBJS32 := build32/libc.o build32/printf.o

.PHONY: all clean test
all: libc-fs.a

libc-fs.a: $(OBJS64)
	$(AR) rcs $@ $(OBJS64)

libc-fs-32.a: $(OBJS32)
	$(AR) rcs $@ $(OBJS32)

build/%.o: %.c libc.h fmt.h
	@mkdir -p $(dir $@)
	$(CC) -m64 $(LIBC_FS_CFLAGS) -c $< -o $@

build/printf.o: deps/printf/printf.c
	@mkdir -p $(dir $@)
	$(CC) -m64 $(LIBC_FS_CFLAGS) -c $< -o $@

build32/%.o: %.c libc.h fmt.h
	@mkdir -p $(dir $@)
	$(CC) -m32 $(LIBC_FS_CFLAGS) -c $< -o $@

build32/printf.o: deps/printf/printf.c
	@mkdir -p $(dir $@)
	$(CC) -m32 $(LIBC_FS_CFLAGS) -c $< -o $@

test: libc-fs.a
	$(CC) -m64 $(LIBC_FS_CFLAGS) -nostdlib -static -no-pie \
	    -o build/libc-fs-test tests/libc_fs_test.c libc-fs.a -lgcc
	./build/libc-fs-test

clean:
	rm -rf build build32 libc-fs.a libc-fs-32.a
