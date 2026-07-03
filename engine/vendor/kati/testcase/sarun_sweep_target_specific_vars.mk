# Systematic sweep: target-specific and pattern-specific variables,
# inheritance by prerequisites, override interplay.
GLOBAL := g
all: T := target-val
all: P += appended
P := base
%.pat: PV := from-pattern
all: sub one.pat
	@echo all_T=[$(T)] all_P=[$(P)]
sub:
	@echo sub_T=[$(T)] sub_GLOBAL=[$(GLOBAL)]
one.pat:
	@echo pat_PV=[$(PV)]
