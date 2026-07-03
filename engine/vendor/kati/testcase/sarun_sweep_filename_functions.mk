# Systematic sweep: filename functions on tricky paths (no slash, trailing
# slash, dotfiles, multiple dots, absolute).
P := a/b/c.x .hidden ../up/f.tar.gz /abs/q noext dir/
test:
	@echo dir=[$(dir $(P))]
	@echo notdir=[$(notdir $(P))]
	@echo suffix=[$(suffix $(P))]
	@echo basename=[$(basename $(P))]
	@echo addsuffix=[$(addsuffix .o,a b)]
	@echo addprefix=[$(addprefix p/,a b)]
	@echo join=[$(join a b c,1 2)] [$(join a,1 2 3)]
