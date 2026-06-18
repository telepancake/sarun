COND := yes
ifeq ($(COND),yes)
export PICKED := picked-yes
endif
all:
	@echo from-recipe=$$PICKED
