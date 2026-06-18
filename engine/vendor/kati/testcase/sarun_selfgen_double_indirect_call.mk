pick = $(if $(filter $1,one),T1,T2)

define T1
RESULT := took-T1-$1
endef

define T2
RESULT := took-T2-$1
endef

$(eval $(call $(call pick,one),hello))
$(info first=$(RESULT))
$(eval $(call $(call pick,other),world))
$(info second=$(RESULT))

all: ; @echo done
