<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Contributing

Thank you for improving conpty-oxide. Correct lifecycle behavior across
Windows releases is the project's first priority; API convenience comes after
ownership, teardown, and cancellation safety.

## Before opening an issue

Use the bug or feature issue form. Do not place vulnerabilities or sensitive
conduct reports in a public issue; follow `SECURITY.md` or
`CODE_OF_CONDUCT.md` instead.

For a bug, include the Windows build (`winver`), process architecture, Rust
version, enabled Cargo features, backend choice, a minimal reproduction, and
the exact exit status or error chain. ConPTY behavior differs materially
between Windows 10, Server 2022, Windows 11 24H2, and standalone bundles.

## Development setup

Development and tests require Windows 10 version 1809 or later. Install
[mise](https://mise.jdx.dev/), then run:

```powershell
just setup
just doctor
```

The project supports Rust 1.75 and the three MSVC Windows targets listed in
`deny.toml`. The setup recipe installs pinned tools and repository hooks.

## Making a change

- Keep the public 0.1 surface centered on managed blocking and Tokio sessions.
  Open an issue before adding advanced lifecycle controls or a platform facade.
- Treat each raw handle and `HPCON` as singly owned. Every unsafe block needs a
  local safety argument, and every callback must prevent unwinding across FFI.
- Test teardown in adversarial drop orders and cancellation points. A passing
  happy-path spawn test is not enough for lifecycle code.
- Add or update documentation and `CHANGELOG.md` for user-visible behavior.
- Use Conventional Commit subjects; the repository hook enforces them.

Run the focused tests while iterating, then before requesting review run:

```powershell
just pre-push
just package-check
```

Maintainers run `just ci` for the complete Windows, MSRV, public API, external
DLL, documentation, and coverage gates. Scheduled mutation testing is a
separate long-running workflow.

## Public API changes

The committed files under `public-api/` are the pre-1.0 contract for four
feature shapes. If a deliberate API change is approved, inspect all generated
surfaces and then run `just public-api-update`. Do not update snapshots merely
to make CI pass.

The `tracing` feature must not change the public API, the default surface must
equal the `blocking` surface, and private dependency types must not leak.

## Pull requests

Keep a pull request focused and explain the Windows versions and lifecycle
states it affects. Complete the template, link the issue when one exists, and
include tests that fail without the change. Reviewers may ask for primary
Microsoft documentation or a minimal Win32 reproduction when behavior is
version-sensitive.

By contributing, you agree that your contribution is licensed under the
project's MIT OR Apache-2.0 terms.
