# .SECONDEXPANSION lets $$@/$$<<etc. expand at build time, not parse time.
.SECONDEXPANSION:
foo.gen: ; @echo built $@
all: foo.gen.dep
all: $$(patsubst %.dep,%,$$@)
all: ; @echo all-done
