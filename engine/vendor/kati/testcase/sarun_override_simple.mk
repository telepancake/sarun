# Run with: make X=ignored — should still print X=set-by-makefile.
override X := set-by-makefile
all:
	@echo X=$(X)
