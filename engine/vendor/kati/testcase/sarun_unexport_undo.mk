export GOO := visible
all:
	@echo before=[$${GOO-unset}]
	@$(MAKE) -f $(firstword $(MAKEFILE_LIST)) inner
unexport GOO
inner:
	@echo inner=[$${GOO-unset}]
