# Sweep 2: -include of a GENERATED dependency file that a rule remakes,
# plus include of multiple files in one directive.
.DEFAULT_GOAL := all
-include gen1.inc gen2.inc
gen1.inc:
	@echo 'V1 := one' > $@
gen2.inc:
	@echo 'V2 := two' > $@
all:
	@echo v=[$(V1)$(V2)]
