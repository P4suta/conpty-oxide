---
date: 2026-10-24
---

<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Heavy audits run on release candidates

## Status

Accepted.
Partly supersedes [ADR 0001](0001-mutation-testing.md).

## Context

Mutation testing uses four Windows shards.
The lifecycle soak uses two Windows runners.
Weekly source audits cannot verify a different release candidate.
Ordinary pull requests already run the deterministic lifecycle tests.

## Decision

Keep mutation testing and lifecycle soak as explicitly dispatched workflows.
Remove their weekly schedules.
Before a release, dispatch both workflows for the candidate ref.
Each successful run must have the exact candidate commit as its head SHA.
A changed candidate needs new results.
Keep the existing commands, shards, supported platforms, and test assertions.
The existing mutation exclusion requirements still apply.

Weekly audits were rejected because their results can describe an older source tree.
Running the campaigns on every pull request was rejected.
The normal lifecycle suite already gates those changes.

## Consequences

Idle repositories no longer repeat source campaigns each week.
Release review must check the audit results and their commit identities.
Manual dispatch also remains available when an individual change needs an audit.
Published-release integrity scanning stays weekly.
External vulnerability data can change without a source change.
