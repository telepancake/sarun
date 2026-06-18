$(shell mkdir -p sub && printf 'inner: ; @echo INNER_OK\n' > sub/Makefile)
all:
	@$(MAKE) -C sub inner
