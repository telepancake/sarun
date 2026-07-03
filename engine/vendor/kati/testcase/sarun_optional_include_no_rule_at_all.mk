# `-include blah` with NO producer anywhere must be silently tolerated —
# a regression once made it fatal ("No rule to make target 'blah'").
all:
	@echo ran-no-producer
-include blah
