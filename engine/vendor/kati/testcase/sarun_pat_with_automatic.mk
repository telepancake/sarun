# TODO(rust)
%.x: TAG := for-$@
all: thing.x
thing.x: ; @echo $@ TAG=$(TAG)
