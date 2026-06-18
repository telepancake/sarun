# Same pattern-spec applied across multiple matching targets.
%.x: LABEL := xy
all: one.x two.x
one.x: ; @echo $@ LABEL=$(LABEL)
two.x: ; @echo $@ LABEL=$(LABEL)
