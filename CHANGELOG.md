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

### Known scope

- Version 0.1 is Windows-only and requires Windows 10 version 1809 or later.
- Detached sessions, manual EOF policy, cursor inheritance, pre-staged spawn,
  and a cross-platform facade are intentionally outside the 0.1 API.

[Unreleased]: https://github.com/P4suta/conpty-oxide/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/P4suta/conpty-oxide/releases/tag/v0.1.0
