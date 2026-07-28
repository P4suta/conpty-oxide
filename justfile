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

# Build the documentation, failing on any rustdoc warning (missing docs included)
doc:
    $env:RUSTDOCFLAGS = '-D warnings'; cargo doc --all-features --no-deps

# Run the test suite; nextest cannot run doctests, so those get their own pass
test:
    cargo nextest run --all-features --no-tests=pass
    cargo test --all-features --doc

# Download the pinned Microsoft.Windows.Console.ConPTY bundle into vendor/conpty
fetch-conpty:
    powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-conpty.ps1

# Run the suite against the vendored conpty.dll as well as the system ConPTY
test-dll: fetch-conpty
    $env:CONPTY_OXIDE_TEST_DLL_DIR = (Join-Path $PWD 'vendor/conpty'); cargo nextest run --all-features --no-tests=pass

# Everything CI checks, in the order CI checks it
ci: lint doc test

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
