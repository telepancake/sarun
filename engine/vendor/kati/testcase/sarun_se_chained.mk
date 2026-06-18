# Two-level chain where the prereq itself uses $$@.
.SECONDEXPANSION:
LEAVES := a b
all: $$(LEAVES)
a: ; @echo build $@
b: ; @echo build $@
all: ; @echo all-done
