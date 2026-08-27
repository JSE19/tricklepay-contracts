# soroban-sdk requires wasm32v1-none on Rust 1.82+; wasm32-unknown-unknown
# enables wasm features the Soroban environment does not support.
WASM_TARGET := wasm32v1-none
WASM := target/$(WASM_TARGET)/release/tricklepay_stream.wasm

.PHONY: all build wasm test fmt fmt-check lint audit clean deploy

all: fmt-check lint test

# Native debug build.
build:
	cargo build

# Optimized WASM artifact for deployment.
wasm:
	cargo build --release --target $(WASM_TARGET)
	@echo "built $(WASM)"

# Run the full test suite.
test:
	cargo test

# Format the workspace in place.
fmt:
	cargo fmt

# Verify formatting without modifying files (used in CI).
fmt-check:
	cargo fmt --check

# Lint every target and treat warnings as errors.
lint:
	cargo clippy --all-targets -- -D warnings

# Audit dependencies while excluding unavoidable Soroban test-host warnings.
# These crates are transitive dependencies and are not used by the WASM
# contract; security advisories remain enabled.
audit:
	cargo audit --deny warnings --no-yanked --ignore RUSTSEC-2024-0388 --ignore RUSTSEC-2024-0436

# Remove build artifacts.
clean:
	cargo clean

# Build, install, and deploy to testnet. Pass an identity: make deploy ID=alice
deploy:
	./scripts/deploy.sh $(ID)
