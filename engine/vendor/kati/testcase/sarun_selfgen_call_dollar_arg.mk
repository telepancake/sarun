SP := $() $()
WORDS := foo bar baz

f = first=$(firstword $1) count=$(words $1)

R := $(call f,$(WORDS))

$(info R=[$(R)])

all: ; @echo "[$(R)]"
