all: a.o
	@echo newer=[$?]
a.o: a.c b.c
	@touch $@
a.c b.c: ; @touch $@
