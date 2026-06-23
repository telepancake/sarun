# TODO(rust)
all:
	@$(foreach v,foo,echo \#define $(v);)
