# An optional include whose rule exists but whose PREREQUISITE has no rule:
# GNU tolerates this too (no error, build proceeds) — the remake attempt
# simply doesn't happen.
all:
	@echo ran-unpickable
-include gen.dep
gen.dep: missing-src
	@cp missing-src $@
