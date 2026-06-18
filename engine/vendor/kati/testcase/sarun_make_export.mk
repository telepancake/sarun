export GREET := hello
all:
	@$(MAKE) -f $(firstword $(MAKEFILE_LIST)) sub
sub:
	@echo sub-sees=$$GREET
