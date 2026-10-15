BIN_DIR ?= $(HOME)/workspace/bin

.PHONY: build test lint install

build:
	cargo build --release

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

# Replace instead of overwrite: rewriting a signed binary in place can get it killed on Apple Silicon.
install: lint test build
	mkdir -p $(BIN_DIR)
	rm -f $(BIN_DIR)/backsync
	cp target/release/backsync $(BIN_DIR)/backsync
	@echo "installed $(BIN_DIR)/backsync"
