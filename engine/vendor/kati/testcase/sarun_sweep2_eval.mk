# Sweep 2: $(eval) — defining rules/vars from expansion, inside foreach,
# and eval'd target-specific vars.
.DEFAULT_GOAL := all
define RULEGEN
$(1).gen:
	@echo made-$(1) > $(1).gen
gen_targets += $(1).gen
endef
$(foreach n,alpha beta,$(eval $(call RULEGEN,$(n))))
$(eval EVDIRECT := from-eval)
all: $(gen_targets)
	@echo direct=[$(EVDIRECT)] targets=[$(gen_targets)]
	@cat alpha.gen beta.gen
