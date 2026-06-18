# Sibling target should NOT see another target's per-target var.
all: a b
a: ONLY_FOR_A := only-a
a:
	@echo a-sees=[$(ONLY_FOR_A)]
b:
	@echo b-sees=[$(ONLY_FOR_A)]
