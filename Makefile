.PHONY: run check

run:
	cargo run

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	actionlint
