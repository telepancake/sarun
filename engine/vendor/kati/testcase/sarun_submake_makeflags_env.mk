# A makefile-level `MAKEFLAGS += <flag>` must reach a child make through the
# ENVIRONMENT — GNU make always exports MAKEFLAGS. The Linux kernel depends
# on this: `MAKEFLAGS += --include-dir=$(abs_srctree)` only takes effect in
# the re-invoked sub-make, via env. The child here is whatever `make`
# resolves on PATH (real GNU make on both sides of the corpus diff); what's
# tested is the PARENT's export.

MAKEFLAGS += --no-print-directory

$(shell printf 'show:\n\t@echo flags=[$$(filter --no-print-directory,$$(MAKEFLAGS))]\n' > child.mk)

test:
	@$(MAKE) -f child.mk show
