.DEFAULT_GOAL := help

APP_NAME := marklogic-tui
VERSION   := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')

# Cross-compilation targets
TARGET_LINUX := x86_64-unknown-linux-musl
TARGET_MAC   := x86_64-apple-darwin
TARGET_MAC_ARM := aarch64-apple-darwin

##@ General

.PHONY: help
help: ## Show this help message
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Development

.PHONY: build
build: ## Build the project in debug mode
	cargo build

.PHONY: run
run: ## Run the application (pass args via ARGS="...")
	cargo run -- $(ARGS)

.PHONY: test
test: ## Run tests
	cargo test

.PHONY: check
check: ## Check code without producing a binary
	cargo check

.PHONY: clippy
clippy: ## Run clippy lints
	cargo clippy -- -D warnings

.PHONY: fmt
fmt: ## Format source code
	cargo fmt

.PHONY: fmt-check
fmt-check: ## Check formatting without modifying files
	cargo fmt -- --check

##@ Release

.PHONY: release
release: ## Build optimised release binary for the current platform
	cargo build --release
	@echo "Binary: target/release/$(APP_NAME)"

.PHONY: release-linux
release-linux: _ensure-zigbuild _ensure-linux-target ## Cross-compile a release binary for Linux (x86_64, musl)
	cargo zigbuild --release --target $(TARGET_LINUX)
	@echo "Binary: target/$(TARGET_LINUX)/release/$(APP_NAME)"

.PHONY: _ensure-zigbuild
_ensure-zigbuild:
	@which zig > /dev/null 2>&1 || (echo "Installing zig via brew..." && brew install zig)
	@cargo zigbuild --version > /dev/null 2>&1 || (echo "Installing cargo-zigbuild..." && cargo install cargo-zigbuild)

.PHONY: _ensure-linux-target
_ensure-linux-target:
	@rustup target list --installed | grep -q $(TARGET_LINUX) || rustup target add $(TARGET_LINUX)

.PHONY: release-mac
release-mac: ## Build a release binary for macOS (x86_64 Intel)
	cargo build --release --target $(TARGET_MAC)
	@echo "Binary: target/$(TARGET_MAC)/release/$(APP_NAME)"

.PHONY: release-mac-arm
release-mac-arm: ## Build a release binary for macOS (Apple Silicon)
	cargo build --release --target $(TARGET_MAC_ARM)
	@echo "Binary: target/$(TARGET_MAC_ARM)/release/$(APP_NAME)"

.PHONY: release-all
release-all: release release-linux release-mac release-mac-arm ## Build release binaries for all supported platforms

##@ Housekeeping

.PHONY: clean
clean: ## Remove build artefacts
	cargo clean

.PHONY: update
update: ## Update dependencies
	cargo update

.PHONY: install
install: ## Install the release binary to ~/.cargo/bin
	cargo install --path .
