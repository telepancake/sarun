all: sub
	@echo top-FOO=$(FOO)
all: FOO := PUB
sub: private FOO := SECRET
sub:
	@echo sub-FOO=$(FOO)
