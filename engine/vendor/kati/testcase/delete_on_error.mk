
.DELETE_ON_ERROR:

test: file

file:
	touch $@
	false
