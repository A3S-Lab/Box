# A3S Box - Justfile
# Run `just --list` to see all available commands
#
# Each crate has its own justfile in src/<crate>/justfile
# Use `just <crate> <command>` to run crate-specific commands

# Default recipe - show help
default:
    @just --list

# AI-powered Commitizen-style commit
cz:
    @bash .scripts/generate-commit-message.sh

# ============================================================================
# Global Commands
# ============================================================================

# Build everything (Rust crates + all SDKs)
build: build-rust sdk-ts-build sdk-python-build
    @echo "All builds complete!"

# Build only Rust crates
build-rust:
    cd src && cargo build --workspace

# Build everything in release mode
build-release: build-rust-release sdk-ts-build sdk-python-build-release
    @echo "All release builds complete!"

# Build only Rust crates in release mode
build-rust-release:
    cd src && cargo build --workspace --release

# Run all tests
test:
    cd src && cargo test --all

# Run all tests with output
test-verbose:
    cd src && cargo test --all -- --nocapture

# Format all code
fmt:
    cd src && cargo fmt --all

# Check formatting
fmt-check:
    cd src && cargo fmt --all -- --check

# Run clippy on all crates
lint:
    cd src && cargo clippy --all-targets --all-features -- -D warnings

# Run clippy with fixes
lint-fix:
    cd src && cargo clippy --all-targets --all-features --fix --allow-dirty

# Clean build artifacts
clean:
    cd src && cargo clean

# Check all crates compile
check:
    cd src && cargo check --all

# Generate documentation
doc:
    cd src && cargo doc --no-deps --document-private-items

# Open documentation in browser
doc-open:
    cd src && cargo doc --no-deps --open

# ============================================================================
# Crate-specific Commands (delegates to per-crate justfiles)
# ============================================================================

# Run core crate commands (just core test, just core build, etc.)
core *ARGS:
    just -f src/core/justfile {{ARGS}}

# Run runtime crate commands
runtime *ARGS:
    just -f src/runtime/justfile {{ARGS}}

# Run code crate commands
code *ARGS:
    just -f src/code/justfile {{ARGS}}

# Run queue crate commands
queue *ARGS:
    just -f src/queue/justfile {{ARGS}}

# Run cli crate commands
cli *ARGS:
    just -f src/cli/justfile {{ARGS}}

# Run python sdk commands
sdk-python *ARGS:
    just -f src/sdk/python/justfile {{ARGS}}

# Run typescript sdk commands
sdk-ts *ARGS:
    just -f src/sdk/typescript/justfile {{ARGS}}

# ============================================================================
# Development Utilities
# ============================================================================

# Watch for changes and rebuild
watch:
    cd src && cargo watch -x build

# Watch for changes and run tests
watch-test:
    cd src && cargo watch -x 'test --all'

# Show dependency tree
deps:
    cd src && cargo tree

# Show outdated dependencies
deps-outdated:
    cd src && cargo outdated

# Update dependencies
deps-update:
    cd src && cargo update

# ============================================================================
# CI/CD Commands
# ============================================================================

# Run all CI checks
ci: fmt-check lint test
    @echo "All CI checks passed!"

# Run pre-commit checks
pre-commit: fmt-check lint check
    @echo "Pre-commit checks passed!"

# ============================================================================
# Proto Generation
# ============================================================================

# Regenerate all proto files
proto:
    cd src && cargo build -p a3s-box-runtime && cargo build -p a3s-box-code

# ============================================================================
# Benchmarks
# ============================================================================

# Run benchmarks (if any)
bench:
    cd src && cargo bench

# ============================================================================
# Installation
# ============================================================================

# Install the CLI locally
install-cli:
    cd src && cargo install --path cli

# Uninstall the CLI
uninstall-cli:
    cargo uninstall a3s-box

# ============================================================================
# Workspace Info
# ============================================================================

# Show workspace structure
info:
    @echo "A3S Box Workspace"
    @echo "================"
    @echo ""
    @echo "Crates:"
    @echo "  - core:           Core types and abstractions"
    @echo "  - runtime:        VM lifecycle and session management"
    @echo "  - code:           Guest agent (runs inside VM)"
    @echo "  - queue:          Queue manager utilities"
    @echo "  - cli:            Command-line interface"
    @echo "  - sdk/python:     Python bindings (PyO3)"
    @echo "  - sdk/typescript: TypeScript bindings (NAPI-RS)"
    @echo ""
    @echo "Per-crate commands:"
    @echo "  just core <cmd>       - Run core crate command"
    @echo "  just runtime <cmd>    - Run runtime crate command"
    @echo "  just code <cmd>       - Run code crate command"
    @echo "  just queue <cmd>      - Run queue crate command"
    @echo "  just cli <cmd>        - Run cli crate command"
    @echo "  just sdk-python <cmd> - Run Python SDK command"
    @echo "  just sdk-ts <cmd>     - Run TypeScript SDK command"
    @echo ""
    @echo "Example: just code test, just runtime build"

# Count lines of code
loc:
    @echo "Lines of Rust code:"
    @find src -name "*.rs" -not -path "*/target/*" | xargs wc -l | tail -1

# Count lines of code per crate
loc-detail:
    @echo "Lines of code per crate:"
    @echo ""
    @for crate in core runtime code queue cli; do \
        echo "  $crate:"; \
        find src/$crate -name "*.rs" -not -path "*/target/*" 2>/dev/null | xargs wc -l 2>/dev/null | tail -1 || echo "    0"; \
    done

# ============================================================================
# Examples
# ============================================================================

# Build the TypeScript SDK (required before using @a3s-lab/box)
sdk-ts-build:
    cd src/sdk/typescript && npm install && npm run build

# Build the Python SDK
sdk-python-build:
    just sdk-python build

# Build the Python SDK in release mode
sdk-python-build-release:
    just sdk-python build-release
