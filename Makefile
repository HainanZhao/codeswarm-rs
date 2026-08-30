.PHONY: run test verify rust-test diff-check

run:
	cargo run -p codeswarm -- $(ARGS)

# Cargo is the canonical test runner for this Rust-only repository.
test:
	cargo test --workspace --locked

rust-test:
	cargo fmt --all -- --check
	cargo test --workspace --locked
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo build --release -p codeswarm --locked
	cargo package --workspace --locked --allow-dirty --no-verify

diff-check:
	git diff --check HEAD

verify: diff-check rust-test
