# Two-tier implicit-rule selection. app.o matches both `%.o: %.c` and
# `%.o: %.S`. The `.c` prerequisite is "available" (app.c is a mentioned
# target), while the `.S` prerequisite is only obtainable by chaining another
# pattern rule (`%.S: %.gen`). GNU make prefers the rule whose prerequisite is
# directly available over one needing an intermediate, so it must compile from
# app.c and never assemble a generated app.S. (This is busybox's
# applets/applets.o: a `%.o: %.S` rule and a catch-all link rule make the bogus
# applets.S look producible, but applets.c exists and must win.)
all: app.o

%.o: %.c
	@echo CC $@ from $<

%.o: %.S
	@echo AS $@ from $<

app.c:
	@echo CREATE $@

%.S: %.gen
	@echo GEN $@ from $<

app.gen:
	@echo CREATE $@
