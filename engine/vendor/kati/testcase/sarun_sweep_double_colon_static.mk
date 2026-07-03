# TODO: double-colon parts must be INDEPENDENT dep nodes — each ::
# rule with its own prereqs/commands, run in declaration order. kati merges
# them into one node, so a part with prerequisites is reordered ahead of
# the command-bearing parts. Needs per-part DepNodes.
# Systematic sweep: double-colon rules (both run, in order) and static
# pattern rules (stem binding).
all:: 
	@echo dc-one
all::
	@echo dc-two
all:: statics
OBJS := x.o y.o
statics: $(OBJS)
$(OBJS): %.o: %.src
	@echo build_$@_from_$<_stem_$*
x.src y.src:
	@: 
