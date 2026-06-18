# Commas inside $(call) args need $(,) protection.
COMMA := ,
join2 = $1$2

# Passing a literal comma as arg2 — must use $(COMMA) escape.
R := $(call join2,first,$(COMMA)second)

$(info R=[$(R)])

all: ; @echo "[$(R)]"
