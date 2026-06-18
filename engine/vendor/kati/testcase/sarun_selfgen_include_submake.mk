# TODO(rust)
# Generated .mk uses $(MAKE) — testing recursive invocation hook-up.
.DEFAULT_GOAL := all

sub.mk:
	echo 'inner: ; @echo from-submake' > inner.mk
	echo 'top: ; +$(MAKE) -f inner.mk inner' > $@

include sub.mk

all: top
	@echo top-done
