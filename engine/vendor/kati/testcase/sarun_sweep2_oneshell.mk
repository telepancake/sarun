# Sweep 2: .ONESHELL — all recipe lines run in ONE shell (state persists).
.ONESHELL:
all:
	@x=hello
	echo one=[$$x]
	cd /tmp
	echo two=[$${PWD}]
