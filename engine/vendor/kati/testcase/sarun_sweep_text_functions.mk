# Systematic sweep: GNU text functions with adversarial inputs — empty
# words, non-matching patterns, %-patterns in filter, repeated words.
V := aa xbb aaxbb bb
test:
	@echo subst=[$(subst bb,YY,$(V))]
	@echo subst_empty_from=[$(subst ,YY,$(V))]
	@echo patsubst=[$(patsubst %xbb,%Q,$(V))]
	@echo patsubst_nomatch=[$(patsubst zz%,Q,$(V))]
	@echo patsubst_plain=[$(patsubst aa,Q,$(V))]
	@echo strip=[$(strip   aa    bb  )]
	@echo findstring=[$(findstring xb,$(V))] [$(findstring zz,$(V))]
	@echo filter=[$(filter %bb aa,$(V))]
	@echo filterout=[$(filter-out %bb,$(V))]
	@echo sort=[$(sort bb aa bb aa)]
	@echo word=[$(word 2,$(V))] words=[$(words $(V))]
	@echo wordlist=[$(wordlist 2,3,$(V))] wl_over=[$(wordlist 3,99,$(V))]
	@echo first=[$(firstword $(V))] last=[$(lastword $(V))]
