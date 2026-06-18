# TODO(rust)
# The canonical "mkdir on demand" idiom: $$(@D)/.mkdir.
.SECONDEXPANSION:
out/x: $$(@D)/.mkdir ; @echo built $@
%/.mkdir: ; @echo mkdir for $(@D)
all: out/x
