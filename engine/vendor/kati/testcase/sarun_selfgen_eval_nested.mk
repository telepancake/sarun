define INNER
INNER_VAR := inside
endef

define OUTER
$$(eval $$(INNER))
OUTER_VAR := outside
endef

$(eval $(OUTER))

$(info INNER_VAR=$(INNER_VAR) OUTER_VAR=$(OUTER_VAR))

all: ; @echo done
