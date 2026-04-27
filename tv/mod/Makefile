obj-m += proctrace.o

CC ?= cc
KDIR    ?= /lib/modules/$(shell uname -r)/build
ROOT    := $(abspath $(CURDIR)/..)
SIGN    := $(KDIR)/scripts/sign-file
MOK_KEY ?= $(ROOT)/MOK.priv
MOK_CER ?= $(ROOT)/MOK.der

MODTRACE_CFLAGS  ?= -std=c11 -O2 -I$(ROOT)
MODTRACE_LDFLAGS ?= -static

ccflags-y += -I$(ROOT)

all: modtrace modules

modules:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

modtrace: modtrace.c $(ROOT)/wire/wire.h
	$(CC) $(MODTRACE_CFLAGS) $(MODTRACE_LDFLAGS) -o $@ modtrace.c

sign: all
	$(SIGN) sha256 $(MOK_KEY) $(MOK_CER) proctrace.ko

load: sign
	-sudo rmmod proctrace 2>/dev/null
	sudo insmod proctrace.ko

unload:
	sudo rmmod proctrace

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
	rm -f modtrace

install: modules
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules_install
	depmod -a

.PHONY: all modules sign load unload clean install
