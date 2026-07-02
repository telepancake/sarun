# GNU make expands shell wildcards in target and prerequisite lists when
# the makefile is read (the Linux kernel relies on this, e.g.
# `xen-hypercalls.h: … $(srctree)/include/xen/interface/xen*.h`).
# A pattern that matches nothing stays literal.

$(shell mkdir -p hdrs)
$(shell touch hdrs/xen1.h hdrs/xen2.h hdrs/other.h)

test: out.txt

out.txt: hdrs/xen*.h
	@echo deps=$^
	@touch $@
