# TODO(rust)
%.x: CFLAGS := -O2
a.x: ; @echo a.x CFLAGS=$(CFLAGS)
b.x: ; @echo b.x CFLAGS=$(CFLAGS)
all: a.x b.x
