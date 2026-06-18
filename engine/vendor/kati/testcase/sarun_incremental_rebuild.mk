# TODO(rust)
# Two sub-make invocations in series: the second pass should be a no-op
# because `thing` already exists and has no newer prereqs. If kati treats
# every file target as out-of-date across invocations (issue #260), the
# recipe runs twice. Expected with GNU make 4.3: "built" prints exactly once.
all:
	@$(MAKE) -s thing
	@sleep 1
	@$(MAKE) -s thing
thing:
	@echo built
	@touch thing
