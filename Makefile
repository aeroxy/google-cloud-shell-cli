.PHONY: build release check run test lint install clean bump-patch bump-minor bump-major update-formula

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ZIP := target/release/gcs-macos-arm64.zip

## Build the project (debug)
build:
	cargo build

## Release build, zipped for the GitHub release (macOS arm64)
release:
	cargo build --release
	rm -f $(ZIP)
	zip -j $(ZIP) target/release/gcs

## Type-check without producing a binary
check:
	cargo check

## Run the CLI; pass subcommands via ARGS
##   make run ARGS="auth status"
##   make run ARGS="ssh uptime"
run:
	cargo run -- $(ARGS)

## Run tests
test:
	cargo test

## Lint
lint:
	cargo clippy --all-targets

## Install the gcs binary into ~/.cargo/bin
install:
	cargo install --path .

## Remove build artifacts
clean:
	cargo clean

## Bump the version in Cargo.toml (0.1.3 → 0.1.4 / 0.2.0 / 1.0.0)
bump-patch bump-minor bump-major:
	@old=$(VERSION); part=$(subst bump-,,$@); \
	new=$$(echo $$old | awk -F. -v p=$$part '{ if (p == "major") print $$1+1 ".0.0"; else if (p == "minor") print $$1 "." $$2+1 ".0"; else print $$1 "." $$2 "." $$3+1 }'); \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	echo "$$old → $$new (next: make release update-formula)"

## Point Formula/gcs.rb at this version's release zip and its SHA256
##   make release update-formula   (then upload that exact zip to the GitHub release)
update-formula:
	@test -f $(ZIP) || { echo "$(ZIP) not found: run make release first"; exit 1; }
	@test $(ZIP) -nt Cargo.toml || { echo "$(ZIP) is older than Cargo.toml: run make release first"; exit 1; }
	@sha=$$(shasum -a 256 $(ZIP) | cut -d' ' -f1); \
	sed -i '' -E "s|/releases/download/[^/]+/|/releases/download/$(VERSION)/|; s/sha256 \"[a-f0-9]*\"/sha256 \"$$sha\"/" Formula/gcs.rb; \
	echo "Formula/gcs.rb → $(VERSION), sha256 $$sha"
