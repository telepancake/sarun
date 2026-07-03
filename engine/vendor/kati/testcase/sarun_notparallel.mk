# .NOTPARALLEL serializes THIS make even under -j — wrapper makefiles rely
# on it to order side-effect files no rule declares as its output. Was a
# stale no-op from before the parallel scheduler existed.
.NOTPARALLEL:
all: consumer
primary:
	@mkdir -p gen && echo lib > gen/side2.so && echo made-primary
consumer: primary gen/side2.so
	@echo "consumer=[$(wordlist 2,2,$^)]"
