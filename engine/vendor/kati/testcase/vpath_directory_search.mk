# GNU make VPATH directory search: a relative prerequisite that doesn't
# exist in the working dir is looked up in the VPATH directories and the
# prerequisite path is rewritten to the found file (so $</$^ see it).
# The Linux kernel's out-of-tree builds (`VPATH := $(srctree)`) resolve
# every source file this way.

$(shell mkdir -p srcdir)
$(shell echo hello > srcdir/foo.c)

VPATH := srcdir

test: out.txt

out.txt: foo.c
	@echo deps=$^ first=$<
	@cat $< > $@
