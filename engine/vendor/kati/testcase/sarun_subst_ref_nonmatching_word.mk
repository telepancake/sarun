# A substitution reference must leave words that do NOT end with the suffix
# untouched — $(V:suf=rep) == $(patsubst %suf,%rep,$(V)). kati rewrote 'aa'
# under ':xbb=bb' into 'aabb' (trim-then-append, unconditionally), which
# corrupted real builds' path lists passed as $(MAKE) cmdline vars.
V1 := aa xbb
V2 := aaxbb bb
V3 := xbb aa
E :=
test:
	@echo S1=[$(V1:xbb=bb)]
	@echo S2=[$(V2:xbb=bb)]
	@echo S3=[$(V3:xbb=bb)]
	@echo SORT=[$(sort $(V1:xbb=bb))]
	@echo APPEND=[$(V1:$(E)=.o)]
