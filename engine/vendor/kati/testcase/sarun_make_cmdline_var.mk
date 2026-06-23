all:
	@echo top X=$(X)
	@$(MAKE) -f $(firstword $(MAKEFILE_LIST)) X=42 sub
sub:
	@echo sub X=$(X)
