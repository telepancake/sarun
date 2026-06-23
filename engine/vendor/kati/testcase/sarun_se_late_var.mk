# Variable referenced via $$ is resolved AFTER the entire makefile parses.
.SECONDEXPANSION:
all: $$(LATE)
LATE := goodbye
goodbye: ; @echo built $@
all: ; @echo all-done
