# Systematic sweep: export/unexport/undefine directives + $(origin) after.
EXP := exported-val
NOEXP := hidden-val
GONE := present
export EXP
unexport NOEXP
undefine GONE
test:
	@echo exp=[$$EXP] noexp=[$$NOEXP]
	@echo gone_origin=[$(origin GONE)] gone_val=[$(GONE)]
