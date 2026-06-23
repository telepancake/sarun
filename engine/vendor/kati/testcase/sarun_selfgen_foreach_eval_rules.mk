all: a b c
	@echo all-done

define MKRULE
$1: ; @echo built-$1
endef

NAMES := a b c

$(foreach n,$(NAMES),$(eval $(call MKRULE,$n)))
