# Systematic sweep: define/endef flavors — recursive template via $(call),
# simply-expanded define (:=), define +=, and blank-line preservation.
blank :=
define n


endef
newline := $(subst $(blank) ,,$(n))
define tmpl
line1 $(1)
line2 $(2)
endef
EAGER := before
define simple :=
now $(EAGER)
endef
EAGER := after
define appended
first
endef
define appended +=
second
endef
test:
	@echo call_tmpl=[$(subst $(newline), / ,$(call tmpl,A,B))]
	@echo simple=[$(simple)]
	@echo appended=[$(subst $(newline), / ,$(appended))]
