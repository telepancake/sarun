# (was TODO) TODO: Fix - "override export define A" is invalid "override" directive.
# sarun: passes with rkati vs GNU make 4.3 — un-TODO'd so the corpus gate enforces it.

# GNU make 4 accepts this syntax. Note kati doesn't agree with make 4
# either.
MAKEVER:=$(shell make --version | grep "Make [0-9]" | sed -E 's/.*Make ([0-9]).*/\1/')
ifeq ($(MAKE)$(MAKEVER),make4)
$(error test skipped)
endif

override export define A
PASS_A
endef
