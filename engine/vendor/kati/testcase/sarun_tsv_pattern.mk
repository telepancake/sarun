%.x: CFLAGS := -O2
out.x: ; @echo CFLAGS=$(CFLAGS) for $@
all: out.x
