<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Changelog

All notable changes to this project will be documented in this file. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
with Cargo's additional pre-1.0 compatibility rules.

## [Unreleased]

## [0.1.2] - 2026-08-03

The public API is unchanged from 0.1.1; every `public-api` snapshot matches
byte for byte.

### Added

- A 64-iteration lifecycle soak repeats large-output teardown, resize/close
  races, managed drop orders, Tokio cancellation, and EOF while enforcing a
  post-warm-up process-handle budget and checking every recorded root and
  grandchild PID is gone. A weekly `soak` workflow runs it on Windows Server
  2022 and 2025, so both the legacy and `ReleasePseudoConsole` teardown paths
  get the repetition. Run it locally with `just soak`. Because the source
  policy forbids ignored tests, the soak is gated on `CONPTY_OXIDE_RUN_SOAK`
  and now says on stderr when it did not run, so a skipped soak can no longer
  be mistaken for a passing one.
- `SUPPORT.md`, a usage-question issue form, and `CODEOWNERS`, matching the
  governance files the sibling Windows crates already ship.

### Changed

- Process command lowering, standard I/O inheritance, attribute lists,
  atomic Job attachment, and synchronous child control now delegate to
  `windows-spawn` 0.1.0 through a one-way internal dependency. ConPTY creation,
  registered waits, Tokio integration, and the public API remain unchanged.
- The unsafe pseudoconsole bridge is now implemented only by a private,
  mutex-guarded spawn capability. A spawn holds the pseudoconsole lifecycle
  mutex for the whole of `CreateProcessW`, so a concurrent resize, clear, or
  close can no longer reach the backend mid-transaction. 0.1.1 handed out the
  raw `HPCON` with no guard. A resize or clear issued from another thread now
  waits for an in-flight spawn, and spawning through a closed pseudoconsole
  reports `ErrorKind::Spawn` with `io::ErrorKind::NotConnected`.
- ConPTY startup now blanks the child's standard handles with `NULL` rather
  than `INVALID_HANDLE_VALUE`, matching Microsoft Terminal's
  `ConptyConnection.cpp`. Both set `STARTF_USESTDHANDLES` and both keep the
  child off standard handles the parent redirected; this aligns the sentinel
  with the reference implementation.

### Fixed

- The README no longer opens with pre-publication bootstrap instructions. That
  paragraph documented the sibling `windows-spawn` checkout above the install
  snippet, and the README ships to crates.io and docs.rs, so it reached
  consumers as if it were usage guidance. It now lives in `CONTRIBUTING.md`.
- A failure to duplicate the root-watcher process handle now terminates the
  session Job, closes and permanently retires the pseudoconsole, and reports a
  spawn-phase error. 0.1.1 could leave the child running on that path.

### Note on mutation testing

This is the first mutation run against the delegated code. It could not have
been done earlier: while the manifest carries `path = "../windows-spawn"`,
cargo-mutants' copy mode cannot resolve the dependency in its temporary tree
and fails at `cargo metadata`, and the weekly workflow does not check out the
sibling either. The run used `--in-place`, as the CI shards do. All 150 mutants
in the delegation surface are now caught; the one survivor,
`Command::get_kill_on_drop`, was a test-only accessor whose test asserted a
single value.

### Note on coverage

Delegating process creation removed `src/command_tests.rs` and
`src/core/proc_tests.rs`, 782 lines that covered command-line quoting,
environment-block construction, and Job attachment. Those behaviors are now
tested in `windows-spawn`. The 92% line, region, and function thresholds still
hold, but they measure a smaller crate than they did in 0.1.1, so the numbers
are not directly comparable across the two releases.

## [0.1.1] - 2026-08-01

### Fixed

- Environment blocks use Windows' native ordinal ignore-case comparison, so
  non-ASCII variable names that Windows treats as distinct are not overwritten
  or removed as aliases.
- A malformed external `conpty.dll` now returns a loader error without showing
  a modal Bad Image dialog, while preserving the caller's thread error mode.
- The legacy root watcher reclaims its registered-wait context and duplicated
  process handle when its post-exit worker cannot be created.
- Bundle version validation keeps the leading digits of a labeled version
  component, so two different labeled builds no longer compare as a
  matching pair.
- The API-boundary assertions no longer render on docs.rs as the crate's
  first — deliberately failing — examples. Every boundary is stated in
  prose and pinned by hidden compile-fail doctests.

## [0.1.0] - 2026-08-01

### Added

- A managed blocking API for creating, controlling, and collecting output from
  Windows pseudoconsole sessions.
- A symmetric Tokio API with cancellation-safe managed session ownership and
  asynchronous named-pipe I/O.
- Root-bounded managed completion that drains while the root runs, preserves
  its real exit status, and terminates descendants that outlive it, including
  after splitting a session into owned parts.
- Consume-style blocking and Tokio `Session::wait` operations that safely drain
  and discard output without an unbounded collection buffer.
- Columns-first `Size::try_new(columns, rows)` terminal dimensions.
- Process-tree termination through a Job object assigned atomically at child
  creation.
- Consistent collection, end-of-file, and teardown behavior on Windows 10 1809
  through current Windows releases.
- Validated loading of a paired `conpty.dll` and `OpenConsole.exe`, plus
  capability detection for optional release and clear operations.
- Public API snapshots, MSRV and architecture gates, external-DLL tests, and
  line, region, and function coverage thresholds.

Version 0.1 is Windows-only and requires Windows 10 version 1809 or later.
Detached sessions, manual EOF policy, cursor inheritance, pre-staged spawn,
and a cross-platform facade are intentionally outside the 0.1 API.

[Unreleased]: https://github.com/P4suta/conpty-oxide/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/P4suta/conpty-oxide/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/P4suta/conpty-oxide/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/P4suta/conpty-oxide/releases/tag/v0.1.0
