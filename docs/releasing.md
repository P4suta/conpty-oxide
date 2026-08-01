<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Release policy

## Public API baseline

The four files under `public-api/` are the reviewed API baseline. Every
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
non-secret variables `RELEASE_PLZ_APP_CLIENT_ID` and `RELEASE_ENABLED`. Keep
`RELEASE_ENABLED` set to `false` except during an explicitly approved publish.
Add the permanent secret
`RELEASE_PLZ_APP_PRIVATE_KEY` for the installed `p4suta-release-plz` App. The
App needs repository **Contents: read/write** and **Pull requests:
read/write**; it does not need Administration access.

The crates.io trusted publisher is owner `P4suta`, repository `conpty-oxide`,
workflow `release-plz.yml`, Environment `release`. Publication uses only the
short-lived OIDC exchange; no registry token exists anywhere.

## Automated release flow

`release-plz` runs only for the exact current `main` commit. Automatic runs
come from a successful `CI` workflow; manual publication additionally queries
Actions for a successful push-triggered CI run at the same SHA. It uses
release pull requests and `release_always = false`, so an unrelated merge
cannot publish. The publish action is not invoked at all unless the release
Environment also has `RELEASE_ENABLED=true`. Its App token is limited to this
repository and the workflow verifies the returned App slug before use.

Once a reviewed release-plz pull request is current and the publish gate is
enabled, release-plz performs the following sequence:

1. `release-plz release` publishes the version, creates `vX.Y.Z`, and creates a
   draft GitHub release.
2. `release-plz.yml` dispatches `release-finalize.yml` at that exact tag.
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
VCS checks bind those artifacts to the tagged commit, so the finalizer's
dispatch ref is not pinned.

If finalization fails before publication, the release remains a mutable draft.
Fix the cause and re-run `release-finalize.yml` at the existing tag. If the
defect is in the finalizer itself, fix it on `main` and dispatch from `main`
with the same inputs: the tag's recorded workflow tree cannot change, and the
workflow checks out the tag it finalizes either way. Never
replace the tag or publish the incomplete draft manually. If publication
succeeded but its eventual immutable-release verification failed, re-run the
same tag: the workflow takes its published, verify-only path.

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
3. Dispatch `release-finalize.yml` at the tag with the matching `tag` and
   `version` inputs. The finalizer repeats the registry checksum and VCS checks
   before it attaches or publishes anything.

Keep the reconciled release as a draft until the finalizer succeeds. The
immutable release, SLSA provenance, CycloneDX attestation, and consumer checks
then follow the normal path.

## Routine releases

Before merging a release-plz pull request:

1. Review the proposed SemVer change and curated `CHANGELOG.md` entry.
2. Run `just release-check` from a clean worktree.
3. Confirm the public API snapshots, package contents, external blocking/Tokio
   consumers, coverage threshold, and dry-run publish all pass.
4. Merge only after the required Ruleset checks pass on the current head.

Keep `RELEASE_ENABLED=false` through the merge. When that exact merge commit's
CI run succeeds, enable the gate, manually dispatch `Release-plz` from `main`,
and return the gate to `false` after the release workflow completes.

After publication, verify the distributed bytes independently:

```powershell
just verify-release v0.1.0
```

The scheduled release-integrity workflow runs the same verification for the
latest immutable release, then feeds the verified SBOM to Grype and uploads its
SARIF results to Code Scanning. This complements Dependabot and `cargo deny`,
which inspect the current source tree rather than the already published crate.
