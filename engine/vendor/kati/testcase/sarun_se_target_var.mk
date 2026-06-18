.SECONDEXPANSION:
all: DEP := bar
all: $$(DEP)
bar: ; @echo built bar
all: ; @echo all-done
