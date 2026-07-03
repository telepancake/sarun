# Systematic sweep: the vpath DIRECTIVE (pattern-scoped search) — distinct
# from the VPATH variable the corpus already covers.
setup := $(shell mkdir -p srcdir && echo payload > srcdir/thing.src)
vpath %.src srcdir
all: gen.out
gen.out: thing.src
	@echo from=[$<]
	@cp $< $@
