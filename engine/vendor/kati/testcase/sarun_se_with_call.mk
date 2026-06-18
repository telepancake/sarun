.SECONDEXPANSION:
prepend = head-$1
target = body
all: $$(call prepend,$$(target))
head-body: ; @echo built $@
all: ; @echo all-done
