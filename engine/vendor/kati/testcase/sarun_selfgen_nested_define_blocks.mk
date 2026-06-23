define OUTER
define INNER
inner-body-$1
endef
outer-result := $$(INNER) $1
endef

$(eval $(call OUTER,X))

$(info INNER=[$(INNER)])
$(info outer-result=[$(outer-result)])

all: ; @echo done
