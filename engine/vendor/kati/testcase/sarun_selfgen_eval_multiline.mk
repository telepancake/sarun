define BODY
X := one
Y := two
endef

$(eval $(BODY))

$(info X=$(X) Y=$(Y))

all: ; @echo X=$(X) Y=$(Y)
