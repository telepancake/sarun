define COND
ifeq ($1,yes)
RESULT_$2 := on
else
RESULT_$2 := off
endif
endef

$(eval $(call COND,yes,A))
$(eval $(call COND,no,B))

$(info A=$(RESULT_A) B=$(RESULT_B))

all: ; @echo $(RESULT_A) $(RESULT_B)
