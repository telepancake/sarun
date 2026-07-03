# Systematic sweep: computed variable names, nested references, $(value),
# $(origin), $(flavor).
xy := INNER
INNER := deep
name := xy
R = rec $(name)
S := simple
test:
	@echo computed=[$($(name))] double=[$($($(name)))]
	@echo 'value=[$(value R)]'
	@echo origin_file=[$(origin xy)] origin_undef=[$(origin NOPE)] origin_auto=[$(origin @)]
	@echo flavor_simple=[$(flavor S)] flavor_rec=[$(flavor R)] flavor_undef=[$(flavor NOPE)]
