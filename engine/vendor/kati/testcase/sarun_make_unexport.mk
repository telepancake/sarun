export SECRET := shhh
unexport SECRET
all:
	@$(MAKE) -f $(firstword $(MAKEFILE_LIST)) sub
sub:
	@echo sub-sees=[$${SECRET-unset}]
