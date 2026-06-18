# TODO(rust)
# Generated .mk introduces a rule (extra:) needed by all — exercises remake+reexec.
gen.mk:
	printf 'extra:\n\t@echo from-generated\n' > $@

include gen.mk

all: extra
	@echo done
