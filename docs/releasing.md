<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Release policy

## Public API baseline

Until `v0.1.0` is published, the four files under `public-api/` are the API
baseline. Every change requires reviewing all four feature shapes and then
running `just public-api-update`. Cargo's pre-1.0 compatibility rules still
apply, but they are not sufficient on their own: a `0.2` API snapshot change
also requires explicit review and approval.

After the `v0.1.0` tag exists, pin `cargo-semver-checks` before preparing the
first patch release. Run it against the tagged baseline separately for no
frontend, `blocking`, `tokio`, and both frontends. Keep the snapshot gate as a
second, human-reviewable check.

## Publication sequence

Publication is blocked until `https://github.com/P4suta/conpty-oxide` exists as
a public repository and the release commit has been pushed. Before the first
release, configure the repository itself:

1. Enable GitHub private vulnerability reporting under **Settings > Security
   > Code security and analysis**. Confirm that the links in `SECURITY.md`,
   `CODE_OF_CONDUCT.md`, and the issue-form chooser open a private report.
2. Protect `main`: require pull requests, require the `CI required` status,
   require the branch to be current before merging, dismiss stale approvals,
   and block force pushes and deletion. Apply the rules to administrators too.
3. Confirm that Actions has read-only repository permissions by default and
   that no workflow receives broader permissions than it needs.
4. Add README badges only after their public endpoints exist. At minimum show
   CI, crates.io, docs.rs, MSRV, and the dual license; click every badge and
   verify that it targets this repository or crate.

For each release:

1. Reconfirm on crates.io that the exact `conpty-oxide` crate name is still
   available (first release) or owned by the expected publishers (later
   releases).
2. Move the pending entries in `CHANGELOG.md` under the version and release
   date, update its comparison links, and verify the Cargo version.
3. Run `just release-check`. It requires a clean worktree, runs the complete
   local CI suite, checks the normalized package and both external consumers,
   and finishes with `cargo publish --dry-run --locked`.
4. Push the release commit and require the full GitHub CI workflow, including
   the published-package job, to pass on that exact commit.
5. Inspect `cargo package --list --locked` once more for secrets, generated
   files, or missing documentation.
6. Create and push the `v0.1.0` tag.
7. Publish with `cargo publish --locked`.
8. Confirm that docs.rs built the tagged version with all features and both
   documented Windows targets. Open several API pages rather than relying only
   on the green build indicator.
9. Build small blocking and Tokio consumers against the registry release and
   confirm spawn, virtual-terminal output, and exit status.
10. Create the GitHub release from the tag using the matching changelog entry.

Do not create the repository, push, tag, or publish as part of an ordinary
release-preparation change.
