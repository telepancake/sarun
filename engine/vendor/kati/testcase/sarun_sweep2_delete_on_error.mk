# Sweep 2: .DELETE_ON_ERROR — a failing recipe's target file is removed.
.DELETE_ON_ERROR:
all: out.bad
	@echo should-not-run
out.bad:
	@echo partial > $@
	@false
