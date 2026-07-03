# Sweep 2: != shell assignment + $(shell) in := context + exit-status var.
LINES != printf 'l1\nl2\n'
NOW := $(shell echo snap)
STATUS := $(.SHELLSTATUS)
FAILS != exit 3; echo never
STATUS2 := $(.SHELLSTATUS)
all:
	@echo lines=[$(LINES)] now=[$(NOW)] s1=[$(STATUS)] s2=[$(STATUS2)]
