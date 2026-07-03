# Systematic sweep: $(call) arg handling ($(0), missing args, nesting,
# recursion), $(foreach) shadowing, $(if)/$(or)/$(and) semantics.
f = [$(0):$(1):$(2)]
outer = $(call f,$(1),via-outer)
rev = $(if $(1),$(call rev,$(wordlist 2,99,$(1))) $(firstword $(1)))
x := shadowed
test:
	@echo call=[$(call f,a,b)] missing=[$(call f,onlyone)]
	@echo nested=[$(call outer,X)]
	@echo recursion=[$(strip $(call rev,1 2 3 4))]
	@echo 'foreach=[$(foreach x,p q,{$(x)})] after=[$(x)]'
	@echo if=[$(if ,then,else)] [$(if nonempty,then,else)] [$(if x,justthen)]
	@echo or=[$(or ,,third)] and=[$(and a,b,c)] and_empty=[$(and a,,c)]
