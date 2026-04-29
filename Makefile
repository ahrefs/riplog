
TARGET_MUSL = x86_64-unknown-linux-musl

build:
	cargo build

release:
	cargo build --release

release-static:
	cargo build --release --features bundled-tzdb --target $(TARGET_MUSL)
	@bin="$${CARGO_TARGET_DIR:-target}/$(TARGET_MUSL)/release/riplog"; \
		ls -lh "$$bin"; file "$$bin"

install:
	cargo install --path=.

fmt:
	cargo fmt

test:
	cargo test

clean:
	cargo clean
