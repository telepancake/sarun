define BODY :=
hi from $(VV)
endef
VV := world
$(info [$(BODY)])
all: ; @:
