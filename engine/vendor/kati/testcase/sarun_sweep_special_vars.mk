# Systematic sweep: special/introspection variables a build system leans on.
FIRST_LIST := $(words $(MAKEFILE_LIST))
.DEFAULT_GOAL := chosen
other:
	@echo wrong-goal
chosen:
	@echo goal=[$(.DEFAULT_GOAL)] cmdgoals=[$(MAKECMDGOALS)]
	@echo makefile_list_nonempty=[$(if $(MAKEFILE_LIST),yes,no)]
	@echo curdir_abs=[$(if $(filter /%,$(CURDIR)),yes,no)]
