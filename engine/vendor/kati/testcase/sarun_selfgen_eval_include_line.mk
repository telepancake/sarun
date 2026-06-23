$(shell echo 'GEN := from-shell' > gen.mk)

$(eval include gen.mk)

$(info GEN=$(GEN))

all: ; @echo $(GEN)
