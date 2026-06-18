V := orig

define BLOCK
override V := overridden-$1
endef

$(eval $(call BLOCK,X))
$(info V=$(V))

all: ; @echo V=$(V)
