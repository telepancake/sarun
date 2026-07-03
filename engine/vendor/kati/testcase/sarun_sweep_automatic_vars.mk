# Systematic sweep: automatic variables incl. D/F variants, $^ dedup vs $+,
# $? (newer-only), $| (order-only separation).
all: dir/t.out
	@echo top_done
dir/t.out: a.dep b.dep a.dep | oo.dep
	@echo at=[$@] lt=[$<] caret=[$^] plus=[$+] pipe=[$|] star=[$*]
	@echo atD=[$(@D)] atF=[$(@F)] ltD=[$(<D)] ltF=[$(<F)]
	@mkdir -p dir && touch dir/t.out
a.dep b.dep oo.dep:
	@touch $@
