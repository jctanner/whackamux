.PHONY: build release run clean test lint fmt check install uninstall

build:
	cargo build

release:
	cargo build --release

run:
	cargo run

clean:
	cargo clean

test:
	cargo test

lint:
	cargo clippy -- -D warnings

fmt:
	cargo fmt

check:
	cargo fmt -- --check
	cargo clippy -- -D warnings
	cargo test

install: release
	cp target/release/whackamux ~/.cargo/bin/

uninstall:
	rm -f ~/.cargo/bin/whackamux
