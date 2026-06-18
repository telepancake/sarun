$(file >gen.mk,FROM_FILE := yes)

include gen.mk

$(info FROM_FILE=$(FROM_FILE))

all: ; @echo $(FROM_FILE)
