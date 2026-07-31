# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
# SPDX-License-Identifier: MIT OR Apache-2.0

set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]

# List available recipes
default:
    @just --list

# Install pinned tools, the pinned PowerShell analyzer, and git hooks
setup: setup-powershell-lint
    $emptyGlobal = Join-Path $PWD '.tools/empty-mise-global.toml'; if (-not (Test-Path -LiteralPath $emptyGlobal)) { New-Item -ItemType File -Path $emptyGlobal | Out-Null }; $env:MISE_GLOBAL_CONFIG_FILE = $emptyGlobal; $env:MISE_CEILING_PATHS = (Split-Path -Parent $PWD.Path); mise --locked install
    lefthook install

# Install PSScriptAnalyzer into the repository-local tool directory
setup-powershell-lint:
    pwsh.exe -NoLogo -NoProfile -Command '$module = Join-Path $PWD ".tools/PSScriptAnalyzer/1.25.0/PSScriptAnalyzer.psd1"; if (-not (Test-Path -LiteralPath $module)) { New-Item -ItemType Directory -Force -Path ".tools" | Out-Null; Save-Module -Name PSScriptAnalyzer -RequiredVersion 1.25.0 -Repository PSGallery -Path ".tools" -Force }'

# Format all Rust sources.
fmt:
    cargo fmt --all
    cargo fmt --manifest-path xtask/Cargo.toml --all

# Enforce repository-wide Rust source rules Clippy cannot express.
source-policy:
    cargo run --manifest-path xtask/Cargo.toml --locked -- source-policy

# Fast, deterministic repository policy checks used by pre-commit.
lint-fast: source-policy
    cargo fmt --all -- --check
    reuse lint
    typos
    taplo fmt --check
    rumdl check .
    yamllint --strict .
    $workflows = @(Get-ChildItem -LiteralPath '.github/workflows' -Filter '*.yml' | Where-Object Name -ne 'release-finalize.yml' | ForEach-Object FullName); actionlint @workflows; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    actionlint -shellcheck= .github/workflows/release-finalize.yml
    cargo run --manifest-path xtask/Cargo.toml --locked -- workflow-shells --path .github/workflows/release-finalize.yml --default-shell bash --expected-count 11
    cargo shear --deny-warnings --check-test-targets

# Strict Rust linting over every feature combination.
lint-rust:
    cargo hack clippy --feature-powerset --all-targets --locked -- -D warnings
    cargo hack clippy --feature-powerset --lib --examples --locked -- -D warnings -D clippy::expect_used -D clippy::unwrap_used -D clippy::panic -D clippy::unreachable -D clippy::wildcard_enum_match_arm -D clippy::let_underscore_must_use -D clippy::map_err_ignore -D clippy::multiple_unsafe_ops_per_block -D clippy::undocumented_unsafe_blocks

# Supply-chain, license, source, and duplicate-dependency policy.
supply-chain:
    cargo deny --all-features --locked check -D unmatched-skip -D unmatched-skip-root

# Analyze every repository PowerShell script with the pinned module.
lint-powershell:
    $module = Join-Path $PWD '.tools/PSScriptAnalyzer/1.25.0/PSScriptAnalyzer.psd1'; if (-not (Test-Path -LiteralPath $module)) { throw 'PSScriptAnalyzer 1.25.0 is missing; run just setup-powershell-lint' }; Import-Module $module -Force; $issues = @(Invoke-ScriptAnalyzer -Path scripts -Recurse -Severity Warning,Error); $issues | Format-Table -AutoSize; if ($issues.Count -ne 0) { throw "PSScriptAnalyzer reported $($issues.Count) issue(s)" }

# Check the contributor automation crate: formatting, lints, and tests.
xtask-check:
    cargo fmt --manifest-path xtask/Cargo.toml --all -- --check
    cargo clippy --manifest-path xtask/Cargo.toml --all-targets --locked -- -D warnings
    cargo test --manifest-path xtask/Cargo.toml --locked

# Every required static policy.
policy: lint-fast supply-chain lint-powershell xtask-check

# Complete lint suite.
lint: lint-fast lint-rust supply-chain lint-powershell xtask-check

# Build the documentation, failing on any rustdoc warning (missing docs included)
doc:
    $env:RUSTDOCFLAGS = '-D warnings'; cargo doc --all-features --no-deps --locked

# Compile the public surface and its examples under every supported feature shape
api-matrix:
    cargo hack check --feature-powerset --all-targets --locked
    cargo hack test --feature-powerset --doc --locked

# Verify the four committed API snapshots and the feature-invariance rules.
public-api:
    powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/check-public-api.ps1

# Deliberately accept the current API as the pre-1.0 baseline.
public-api-update:
    powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/check-public-api.ps1 -Update

# Build the docs as docs.rs does - nightly, --cfg docsrs (needs a nightly toolchain)
doc-docsrs:
    $env:RUSTDOCFLAGS = '--cfg docsrs -D warnings'; cargo +nightly-2026-07-02 doc --all-features --no-deps --locked

# Run the test suite; nextest cannot run doctests, so those get their own pass
test:
    cargo nextest run --all-features --locked --no-tests=pass
    cargo test --all-features --doc --locked

# Run the non-fail-fast CI test profile and generate JUnit.
test-ci:
    cargo nextest run --all-features --profile ci --locked --no-tests=pass
    cargo test --all-features --doc --locked

# Download the pinned Microsoft.Windows.Console.ConPTY bundle into vendor/conpty
fetch-conpty:
    powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/fetch-conpty.ps1

# Run the suite against the vendored conpty.dll as well as the system ConPTY
test-dll: fetch-conpty
    $env:CONPTY_OXIDE_TEST_DLL_DIR = (Join-Path $PWD 'vendor/conpty'); cargo nextest run --all-features --locked --no-tests=pass

# Compile every feature shape with the declared minimum supported Rust version.
msrv:
    cargo +1.75.0 hack check --feature-powerset --all-targets --locked

# Cross-compile all features for every supported Windows architecture.
cross-targets:
    cargo check --all-features --locked --target x86_64-pc-windows-msvc
    cargo check --all-features --locked --target i686-pc-windows-msvc
    cargo check --all-features --locked --target aarch64-pc-windows-msvc

# Produce honest all-feature line, region, and function coverage.
coverage: fetch-conpty
    cargo llvm-cov clean --workspace
    New-Item -ItemType Directory -Path target/llvm-cov -Force | Out-Null
    $env:CONPTY_OXIDE_TEST_DLL_DIR = (Join-Path $PWD 'vendor/conpty'); cargo llvm-cov nextest --all-features --locked --no-report
    cargo llvm-cov report --lcov --output-path target/llvm-cov/lcov.info --fail-under-lines 92 --fail-under-regions 92 --fail-under-functions 92
    cargo llvm-cov report --html --output-dir target/llvm-cov/html

# List mutation candidates without changing source files.
mutants-list:
    cargo mutants --list

# Run the complete mutation suite in safe copy mode.
mutants: fetch-conpty
    $env:CONPTY_OXIDE_TEST_DLL_DIR = (Join-Path $PWD 'vendor/conpty'); cargo mutants

# Run one CI mutation shard in its disposable checkout.
mutants-ci shard: fetch-conpty
    $env:CONPTY_OXIDE_TEST_DLL_DIR = (Join-Path $PWD 'vendor/conpty'); cargo mutants --in-place --shard {{ shard }}/4 --timeout 90 --build-timeout 180 --no-shuffle -vV

# Build and smoke-test the exact normalized source Cargo would publish.
package-check:
    powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File scripts/check-package.ps1

# Verify the latest immutable release, or the supplied v-prefixed tag.
verify-release tag='':
    pwsh.exe -NoLogo -NoProfile -File scripts/verify-release.ps1 -Tag '{{ tag }}'

# Refuse to release from a commit that does not exactly match the worktree.
clean-worktree:
    $changes = @(git status --porcelain=v1 --untracked-files=all); if ($LASTEXITCODE -ne 0) { throw "git status failed with exit code $LASTEXITCODE" }; if ($changes.Count -ne 0) { $changes | Write-Output; throw 'the release worktree is not clean' }

# Every local gate immediately before tagging and publishing.
release-check: clean-worktree ci package-check
    cargo publish --dry-run --locked

# Fast local hook.
pre-commit: lint-fast

# Strict local hook; expensive platform/MSRV/coverage checks stay in CI.
pre-push: lint-rust supply-chain lint-powershell xtask-check test

# Everything required by pull requests; mutation remains scheduled/manual.
ci: lint api-matrix public-api doc doc-docsrs test msrv cross-targets test-dll coverage

# Verify that all required tools are available
doctor:
    pwsh.exe --version
    rustc --version
    rustc +nightly-2026-07-02 --version
    cargo --version
    cargo fmt --version
    cargo clippy --version
    cargo hack --version
    cargo nextest --version
    cargo public-api --version
    cargo deny --version
    cargo shear --version
    cargo llvm-cov --version
    cargo mutants --version
    mise --version
    just --version
    lefthook version
    reuse --version
    typos --version
    committed --version
    taplo --version
    rumdl --version
    yamllint --version
    actionlint --version
    shellcheck --version
