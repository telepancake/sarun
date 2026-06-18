# TODO(rust)
# Two pattern-specific vars; the more-specific match wins for foo.x.
%.x: FLAVOR := generic
f%.x: FLAVOR := specific
all: foo.x bar.x
foo.x: ; @echo $@ FLAVOR=$(FLAVOR)
bar.x: ; @echo $@ FLAVOR=$(FLAVOR)
