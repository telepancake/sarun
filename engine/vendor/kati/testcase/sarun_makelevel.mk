all:
	@echo top=$(MAKELEVEL)
	@$(MAKE) -f $(firstword $(MAKEFILE_LIST)) sub
sub:
	@echo sub=$(MAKELEVEL)
