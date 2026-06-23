-include nonexistent.mk
-include also_missing.d

$(info post-include)

all: ; @echo ok
