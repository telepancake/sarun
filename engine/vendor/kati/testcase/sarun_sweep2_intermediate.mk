# Sweep 2: implicit-rule chains — the intermediate file is built through
# a two-step %-rule chain (.z <- %.y <- %.x); .PRECIOUS keeps it.
.PRECIOUS: %.y
all: thing.z
	@echo have=[$(wildcard thing.y)]
%.z: %.y
	@cp $< $@
%.y: %.x
	@echo mid > $@
thing.x:
	@touch $@
