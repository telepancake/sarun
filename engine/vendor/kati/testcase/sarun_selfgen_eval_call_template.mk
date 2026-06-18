define RULE
$1_OUT := built-from-$1
endef

$(eval $(call RULE,foo))
$(eval $(call RULE,bar))

$(info foo_OUT=$(foo_OUT) bar_OUT=$(bar_OUT))

all: ; @echo $(foo_OUT) $(bar_OUT)
