all: test.a

%.a: %.b
	@echo making $@ from $<

%.b:
	@echo making $@
