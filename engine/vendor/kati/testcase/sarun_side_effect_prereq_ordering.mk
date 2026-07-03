# A rule-less prerequisite created as a SIDE EFFECT of an earlier recipe
# (another rule's undeclared output, or an earlier sub-make) must be found
# on disk when its consumer is considered — GNU checks lazily, in order.
# An eager t=0 existence check errors before anything has built.
all: consumer
primary:
	@mkdir -p gen && echo lib > gen/side.so && echo made-primary
consumer: primary gen/side.so
	@echo "consumer=[$(wordlist 2,2,$^)]"
