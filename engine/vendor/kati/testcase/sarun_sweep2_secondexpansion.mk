# Sweep 2: .SECONDEXPANSION — $$(...) prereqs re-expanded with automatic
# vars bound per-target.
.SECONDEXPANSION:
deps_alpha := a.x
deps_beta := b.x
all: alpha.t beta.t
	@echo done
%.t: $$(deps_$$*)
	@echo built=[$@] from=[$<]
	@touch $@
a.x b.x:
	@touch $@
