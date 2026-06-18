all: child
all: VAR := from-all
child:
	@echo child-sees=$(VAR)
