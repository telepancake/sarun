# Remake-the-makefile where the required include is produced by a PATTERN
# rule, not a literal one — the Linux kernel regenerates its required
# `include include/config/auto.conf` through
# `%/auto.conf %/auto.conf.cmd: $(KCONFIG_CONFIG)` (syncconfig). The
# remake loop must accept pattern-rule producers; matching only literal
# rule outputs fails this with "gen.conf: No such file or directory".

%.conf:
	echo 'CONF := yes' > $@

include gen.conf

all: ; @echo done CONF=$(CONF)
