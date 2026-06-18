# TODO(rust)
FLAGS := -base
%.x: FLAGS += -extra
all: foo.x
foo.x: ; @echo $@ FLAGS=$(FLAGS)
