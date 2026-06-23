VAR := hello
export VAR
all:
	@echo from-recipe=$$VAR
