set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# List available recipes
default:
    @just --list

# Install pinned tools and git hooks
setup:
    mise install
    lefthook install

# Format all Rust sources
fmt:
    cargo fmt --all

# Formatting, clippy (all feature combinations), typos and TOML style checks
lint:
    cargo fmt --all -- --check
    cargo clippy --no-default-features --all-targets -- -D warnings
    cargo clippy --all-targets -- -D warnings
    cargo clippy --features tokio --all-targets -- -D warnings
    cargo clippy --all-features --all-targets -- -D warnings
    typos
    taplo fmt --check

# Run the test suite
test:
    cargo nextest run --all-features --no-tests=pass

# Everything CI checks
ci: lint test

# Verify that all required tools are available
doctor:
    rustc --version
    cargo --version
    cargo fmt --version
    cargo clippy --version
    cargo nextest --version
    mise --version
    just --version
    lefthook version
    typos --version
    committed --version
    taplo --version
