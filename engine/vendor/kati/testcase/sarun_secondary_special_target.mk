# TODO(rust)
.SECONDARY: inter.x
all: final.x
final.x: inter.x
	@cp $< $@
inter.x:
	@echo data > $@
