all:
	@echo $(if $(shell echo),FAIL,PASS)
