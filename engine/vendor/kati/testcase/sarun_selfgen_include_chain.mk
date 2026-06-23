# Two-level remake: a.mk emits an include of b.mk, b.mk defines B.
a.mk:
	echo 'A := from-a' > $@
	echo 'include b.mk' >> $@

b.mk:
	echo 'B := from-b' > $@

include a.mk

$(info A=$(A) B=$(B))

all: ; @echo done
