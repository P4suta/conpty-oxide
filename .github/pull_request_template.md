<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Pull request

## Summary

<!-- What changes, and why is this the smallest safe change? -->

## Lifecycle impact

<!-- Cover handle/HPCON ownership, EOF, drop order, Job behavior, callbacks,
Tokio cancellation, DLL loading, and affected Windows versions as applicable. -->

## Verification

- [ ] I added or updated tests that fail without this change.
- [ ] I ran the relevant focused tests and `just pre-push`.
- [ ] I ran `just package-check` when package contents or public usage changed.
- [ ] I reviewed all unsafe changes and documented each local safety contract.
- [ ] I updated user documentation and `CHANGELOG.md` when behavior changed.
- [ ] I reviewed every public API snapshot; approved changes use `just public-api-update`.
- [ ] This pull request contains no secrets, vulnerability details, or
  sensitive conduct evidence.

## Related issue

<!-- Fixes #123, or explain why no issue is needed. -->
