define TMPL
RES := $(shell echo eager)
endef

$(eval $(value TMPL))

$(info RES=$(RES))

all: ; @echo RES=$(RES)
