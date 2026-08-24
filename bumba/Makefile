.DEFAULT_GOAL := build

.PHONY: vendor build test
vendor:
	python3 tools/vendor.py

build: vendor
	cargo build --locked

test: vendor
	cargo test --locked
