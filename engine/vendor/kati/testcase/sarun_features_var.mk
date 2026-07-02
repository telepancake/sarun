# GNU make advertises optional features via the special .FEATURES variable.
# Makefiles gate on it — the Linux kernel's top Makefile checks
# `$(filter undefine,$(.FEATURES))` and refuses to build when it's empty.
# Pin the tokens kbuild (and common makefiles) rely on.

test:
	@echo undefine=[$(filter undefine,$(.FEATURES))]
	@echo tsv=[$(filter target-specific,$(.FEATURES))]
	@echo oo=[$(filter order-only,$(.FEATURES))]
	@echo se=[$(filter second-expansion,$(.FEATURES))]
