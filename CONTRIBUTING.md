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
mise install
just setup
just doctor
```

`mise install` provisions the pinned tools — including `just` itself — so it
must run before the first `just` invocation.

The project supports Rust 1.75 and the three MSVC Windows targets listed in
`deny.toml`. The setup recipe installs pinned tools and repository hooks.

### Sibling `windows-spawn` checkout (temporary)

Until `windows-spawn` 0.1.0 is published, building from source requires
`conpty-oxide/` and `windows-spawn/` to share the same parent directory. The
manifest carries both `version = "0.1.0"` and `path = "../windows-spawn"`;
Cargo drops the path from the normalized package while retaining the future
registry requirement, and `just paired-package-check` verifies that the
normalized manifest keeps the version and loses the path.

This is a one-way pre-publication bootstrap, not a reciprocal repository
contract — `windows-spawn` neither checks out nor pins `conpty-oxide`. Once
`windows-spawn` 0.1.0 is on crates.io, remove the path override, drop the
sibling-layout requirement, and retire `paired-package-check`; `just
package-check` then covers packaging on its own.

## Making a change

- Keep the public 0.1 surface centered on managed blocking and Tokio sessions.
  Open an issue before adding advanced lifecycle controls or a platform facade.
- Treat each raw handle and `HPCON` as singly owned. Every unsafe block needs a
  local safety argument, and every callback must prevent unwinding across FFI.
- Prefer static dispatch throughout project-owned code. The trait object
  required by the standard `Error::source` method is the sole exception.
- Test teardown in adversarial drop orders and cancellation points. A passing
  happy-path spawn test is not enough for lifecycle code.
- Add or update documentation and `CHANGELOG.md` for user-visible behavior.
- Use Conventional Commit subjects; the repository hook enforces them.

Run the focused tests while iterating, then before requesting review run:

```powershell
just pre-push
just package-check
```

CI runs those gates — Windows matrix, MSRV, public API, external DLL,
documentation, coverage, plus the package smoke test — on every pull request;
`just ci` reproduces them locally. Mutation testing runs as a separate
scheduled workflow under the policy in
[ADR 0001](docs/adr/0001-mutation-testing.md).

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
