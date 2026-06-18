# TODO(rust)
.SECONDEXPANSION:
DEP := after
all: $$(DEP)
	@echo all-done
before: ; @echo making-before
after: ; @echo making-after
DEP := before
