# An optional include whose remake RECIPE fails: GNU proceeds without it.
all:
	@echo ran-after-fail
-include blah2
blah2:
	@false
