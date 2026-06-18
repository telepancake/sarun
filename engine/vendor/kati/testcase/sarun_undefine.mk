A := hi
$(info before flavor=[$(flavor A)] value=[$(A)])
undefine A
$(info after  flavor=[$(flavor A)] value=[$(A)])
all: ; @:
