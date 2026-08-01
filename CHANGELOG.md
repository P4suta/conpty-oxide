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

[Unreleased]: https://github.com/P4suta/conpty-oxide/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/P4suta/conpty-oxide/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/P4suta/conpty-oxide/releases/tag/v0.1.0
