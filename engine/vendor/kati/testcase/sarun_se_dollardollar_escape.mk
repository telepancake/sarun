# $$$$ in a prereq survives both expansions and becomes $$ on the shell.
# (Sanity test: with SECONDEXPANSION on, only $$ survives parse pass 1.)
.SECONDEXPANSION:
DEP := plain
all: $$(DEP)
plain: ; @echo built $@
all: ; @echo all-done
