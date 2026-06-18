all:
	@$(MAKE) -f $(firstword $(MAKEFILE_LIST)) sub
sub:
	@echo HAS_A=[$${A-unset}] HAS_B=[$${B-unset}]
export A := exported-A
B := not-exported-B
