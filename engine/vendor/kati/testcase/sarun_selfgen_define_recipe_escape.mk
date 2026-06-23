all: foo
	@echo done

define RULE
$1:
	@echo target=$$@ from=$1
endef

$(eval $(call RULE,foo))
