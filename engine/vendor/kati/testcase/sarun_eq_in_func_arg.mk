all: FOO=$(if 1,bar,baz)
all:
	@echo FOO=$(FOO)
