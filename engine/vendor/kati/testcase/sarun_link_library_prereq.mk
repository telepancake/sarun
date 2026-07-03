# GNU link-library prerequisites: `-lNAME` is resolved through
# $(.LIBPATTERNS) (default lib%.so lib%.a) against the working dir, the
# VPATH/vpath search path, and the system lib dirs. Unimplemented, it
# surfaced as "No rule to make target '-lfoo'" on real builds.
setup := $(shell mkdir -p libs && touch libs/libfoo.so && touch libbar.a)
vpath %.so libs
prog: -lfoo -lbar
	@echo "link=[$^]"
