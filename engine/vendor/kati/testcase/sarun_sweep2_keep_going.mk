# Sweep 2: make -k via MAKEFLAGS += -k — independent targets keep building
# after one fails; overall exit is still non-zero.
MAKEFLAGS += -k
all: bad good1 good2
bad:
	@false
good1:
	@echo good1-ran
good2:
	@echo good2-ran
