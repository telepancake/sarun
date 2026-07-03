# Systematic sweep: conditional directive forms — parens vs quotes, mixed,
# ifdef on empty-valued vs undefined, else-if chains, nesting.
EMPTY :=
SET := yes
test:
	@echo done=[$(RESULT)]
ifeq ($(SET),yes)
RESULT += p1
endif
ifeq "$(SET)" "yes"
RESULT += p2
endif
ifeq '$(SET)' 'yes'
RESULT += p3
endif
ifneq ($(SET),no)
RESULT += p4
endif
ifdef EMPTY
RESULT += bad-empty-is-defined
else
RESULT += p5
endif
ifdef UNDEFINED
RESULT += bad
else ifeq ($(SET),yes)
RESULT += p6
else
RESULT += bad2
endif
