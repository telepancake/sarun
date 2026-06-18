# Defining a new define inside a $(eval $(call ...)) expansion.
define MAKER
define G_$1
generated-body-$1
endef
endef

$(eval $(call MAKER,one))
$(eval $(call MAKER,two))

$(info G_one=[$(G_one)])
$(info G_two=[$(G_two)])

all: ; @echo done
