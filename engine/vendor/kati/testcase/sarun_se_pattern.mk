# TODO(rust)
.SECONDEXPANSION:
%.o: %.c ; @echo compile $< to $@
%.c: ; @echo gen $@
all: foo.o
	@echo all-done
