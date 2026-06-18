define greet
hello $1, you are $2
endef

R1 := $(call greet,world,here)
R2 := $(call greet,Bob,there)

$(info R1=$(R1))
$(info R2=$(R2))

all: ; @echo $(R1) / $(R2)
