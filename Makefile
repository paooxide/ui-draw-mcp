.PHONY: all build release test check fmt install docs

all: build

build:
	cargo build --workspace

release:
	cargo build --release -p agentctl

test:
	AGENTCTL_SKIP_LIVE=1 cargo test --workspace

check:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo fmt --all --check

fmt:
	cargo fmt --all

install:
	./scripts/install.sh

docs:
	cargo run -q -p agentctl -- tools --markdown --all > docs/tools.md
