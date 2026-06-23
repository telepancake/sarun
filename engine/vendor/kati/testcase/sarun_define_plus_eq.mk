BODY := one
define BODY +=
two
three
endef
$(info [$(BODY)])
all: ; @:
