<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Mutation testing

Mutation testing is a release audit, not a routine pull-request gate. Run it
with the vendored standalone backend available:

```powershell
$env:CONPTY_OXIDE_TEST_DLL_DIR = Join-Path $PWD 'vendor/conpty'
cargo mutants --shard 0/4 --timeout 90 --build-timeout 180 --no-shuffle -vV
```

Use shards `0/4` through `3/4` in parallel. A mutant that changes behaviour
must be caught by a focused test. Record an exclusion only when the mutated
expression is provably identical for every input; do not exclude files or
classes of mutations.

## v0.1 audit

The pre-release audit enumerated 599 candidates and exercised all of them once
in four copy-mode shards. The first pass classified 303 as caught, 97 as
missed, 198 as unviable, and one as timed out. The missed set drove focused
tests for loader-reference release, DLL search flags, architecture/version
selection, process startup handles, attribute-list destruction, child status
caching, callback and tracing paths, pseudoconsole final-drop routing,
blocking and Tokio EOF notification, cancellation, managed-session delegates,
and named-pipe retry/connect branches.

The timeout replaced `ChildCore::kill` with success without terminating the
Job. Multiple kill-tree tests failed as intended, but nextest needed 67 seconds
to cancel the other concurrent process tests and report the result. The CI
mutation budget is therefore 90 seconds: a focused rerun classified this
mutant as caught rather than timed out.

A 201-candidate focused pass over the changed lifecycle and Win32 boundaries
then reported 163 caught, 32 unviable, five missed, and the same timeout. Four
misses were genuine test gaps in blocking EOF notification, Tokio managed
shutdown, and both named-pipe retry branches. After adding direct behavioural
tests, a copy-mode rerun caught all five affected mutants; the remaining miss
was the equivalent zero-valued flag expression documented below.

## Equivalent mutations

`.cargo/mutants.toml` excludes only these exact cases:

- `Pty::builder` returning `PtyBuilder::default()` versus
  `Default::default()` selects the same unique `Default` implementation.
- The four Tokio `poll_flush` implementations already return the exact
  replacement, `Poll::Ready(Ok(()))` / `Poll::from(Ok(()))`. These streams add
  no userspace buffer, and their direct no-op contract is separately tested.
- Replacing bitwise OR with XOR in the external-DLL search flags is identical:
  `0x100` and `0x800` have no common bit.
- Replacing bitwise OR with XOR in the process creation flags is identical:
  `EXTENDED_STARTUPINFO_PRESENT` and `CREATE_UNICODE_ENVIRONMENT` have no
  common bit.
- Both named-pipe server access flags and the file-mode flags occupy disjoint
  bits. The byte/read/wait mode constants are zero, so OR and XOR also produce
  the same local-only mode value.
- In `pipe_mode`, the first OR combines `PIPE_TYPE_BYTE` and
  `PIPE_READMODE_BYTE`, both of which are zero. Replacing only that operator
  with AND therefore still produces zero. The exclusion is restricted to
  column 20 of that function; the other AND substitutions change the policy
  and remain tested.

The tests assert the resulting numeric policies. All other AND substitutions
and dropped flags remain included and must fail.
