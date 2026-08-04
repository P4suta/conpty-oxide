<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Release policy

## Public API baseline

The four snapshot files under `public-api/` are the reviewed API baseline. Every
change requires reviewing all four feature shapes and then running
`just public-api-update`. Cargo's pre-1.0 compatibility rules still apply,
but they are not sufficient on their own: a `0.2` API snapshot change also
requires explicit review and approval.

Pin `cargo-semver-checks` before preparing the first patch release. Run it
against the tagged `v0.1.0` baseline separately for no frontend, `blocking`,
`tokio`, and both frontends. Keep the snapshot gate as a second,
human-reviewable check.

## Repository setup

The repository uses Rulesets rather than classic branch protection. The
default branch requires pull requests, linear history, current successful CI,
CodeQL for Rust and Actions, Dependency Review, and the configured code-scanning
threshold. A separate Ruleset prevents updates, force pushes, or deletion of
`v*` tags after creation.

Keep these repository settings enabled:

- read-only default `GITHUB_TOKEN` permissions and SHA-pinned selected Actions;
- Dependabot alerts and security updates, secret scanning, push protection,
  private vulnerability reporting, and CodeQL advanced setup;
- immutable releases, squash-only merges, automatic branch deletion, and
  automatic branch updates.

The `release` Environment permits only `main` and `v*`. It contains the
non-secret variable `RELEASE_PLZ_APP_CLIENT_ID` and the permanent secret
`RELEASE_PLZ_APP_PRIVATE_KEY` for the installed `p4suta-release-plz` App. The
App needs repository **Contents: read/write** and **Pull requests:
read/write**; it does not need Administration access. Its required reviewer is
the release gate: nothing is published until that deployment is approved.

The crates.io trusted publisher is owner `P4suta`, repository `conpty-oxide`,
workflow `release-plz.yml`, Environment `release`. Publication uses only the
short-lived OIDC exchange; no registry token exists anywhere.

## Automated release flow

`release-plz.yml` is the workflow release-plz documents, with two deviations it
also documents: a GitHub App token, because the default `GITHUB_TOKEN` cannot
trigger the CI that must run on a release pull request, and `environment:
release` on the publishing job, because the crates.io trusted publisher is
bound to that environment.

It runs on every push to `main`. `release_always = false` and release pull
requests mean an unrelated merge cannot publish: release-plz acts only when the
manifest version on `main` is ahead of the registry. The required reviewer on
the `release` environment is what authorises each publish.

Once a reviewed release-plz pull request is merged and the deployment is
approved, the workflow performs the following sequence:

1. `release-plz release` publishes the version, creates `vX.Y.Z`, and creates a
   draft GitHub release.
2. The `finalize` job calls `release-finalize.yml` with that tag and version.
   It is a called workflow rather than a release-event handler because GitHub
   fires no release events for drafts, and this repository uses immutable
   releases, so the assets must be attached before the draft is published.
3. The finalizer downloads the registry-hosted `.crate`, checks its SHA-256
   against the crates.io sparse index, and checks the archive's Cargo metadata
   and VCS commit.
4. It generates a CycloneDX 1.5 SBOM from the extracted registry artifact,
   binds the distribution URL and digest into the root component, and validates
   the document with the official CycloneDX CLI.
5. GitHub creates SLSA provenance and CycloneDX SBOM attestations. The finalizer
   uploads the `.crate`, SBOM, checksum file, and offline attestation bundles to
   the draft.
6. A clean download is checked against every digest and both online and offline
   attestations. Only then is the draft published as an immutable release.
7. The finalizer consumes GitHub's release attestation with `gh release verify`
   and verifies every release asset.

The SLSA predicate records the hosted finalizer that authenticated and promoted
the crates.io artifact. It is not a claim of an independent reproducible build
or of a particular SLSA build level. Attestation verification binds the signing
workflow, the repository, and the artifact digests; the registry checksum and
VCS checks bind those artifacts to the tagged commit.

If finalization fails before publication, the release remains a mutable draft.
The crate is already on crates.io at that point, so never publish again and
never replace the tag. Fix the cause on `main` and re-run the failed
`Release-plz` run from the Actions UI; the `release` job is idempotent when the
version is already published, and the finalizer re-derives everything from the
tag. If publication succeeded but its eventual immutable-release verification
failed, the same re-run takes the published, verify-only path.

## Registry and GitHub reconciliation

`cargo publish` is irreversible, while creation of the Git tag or draft GitHub
release happens afterward. If release-plz reports failure after crates.io has
accepted the crate, do not publish again and do not move an existing tag.

1. Download the registry `.crate`, require its SHA-256 to equal the exact
   version entry in the crates.io sparse index, and require its
   `.cargo_vcs_info.json` commit to equal the reviewed release commit.
2. If and only if both checks pass, create the missing `vX.Y.Z` tag at that
   exact commit and/or create the missing draft GitHub release for that tag.
   Stop instead of reconciling if any existing tag or release points elsewhere.
3. Re-run the `Release-plz` run for that commit. Its `release` job is
   idempotent once the version is published, and the `finalize` job repeats the
   registry checksum and VCS checks before it attaches or publishes anything.

Keep the reconciled release as a draft until the finalizer succeeds. The
immutable release, SLSA provenance, CycloneDX attestation, and consumer checks
then follow the normal path.

## Routine releases

Before merging a release-plz pull request:

1. Review the proposed SemVer change and curated `CHANGELOG.md` entry.
2. Run `just release-check` from a clean worktree.
3. Confirm the public API snapshots, package contents, external blocking/Tokio
   consumers, coverage threshold, and dry-run publish all pass.
4. Confirm the latest scheduled mutation audit is green, or dispatch one for
   a release that changed lifecycle or Win32 boundary code.
5. Merge only after the required Ruleset checks pass on the current head.

Merging the release pull request starts `Release-plz` automatically. It stops
at the `release` environment for approval; approve it once the merge commit's
CI is green.

After publication, verify the distributed bytes independently:

```powershell
just verify-release v0.1.0
```

The scheduled release-integrity workflow runs the same verification for the
latest immutable release, then feeds the verified SBOM to Grype and uploads its
SARIF results to Code Scanning. This complements Dependabot and `cargo deny`,
which inspect the current source tree rather than the already published crate.
