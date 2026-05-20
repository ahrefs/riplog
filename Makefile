
TARGET_MUSL = x86_64-unknown-linux-musl

build:
	cargo build

release:
	cargo build --release

release-static:
	cargo build --release --features bundled-tzdb --target $(TARGET_MUSL)
	@bin="$${CARGO_TARGET_DIR:-target}/$(TARGET_MUSL)/release/riplog"; \
		ls -lh "$$bin"; file "$$bin"

build-profiling:
	cargo build --profile=profiling

install:
	cargo install --path=.

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

bench-perf:
	python tests/bench.py

bench-perf-update-baseline:
	python tests/bench.py --update-baseline

test:
	cargo test

clean:
	cargo clean
