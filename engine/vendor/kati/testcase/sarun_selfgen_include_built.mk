# TODO(rust)
# Tests make's remake-the-makefile loop: the included .mk is built first
# (echoing the recipe), then make re-execs and the second-pass $(info) sees GEN.
gen.mk:
	echo 'GEN := hello' > $@

include gen.mk

$(info GEN=$(GEN))

all: ; @echo done GEN=$(GEN)
