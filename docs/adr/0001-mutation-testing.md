<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>

SPDX-License-Identifier: MIT OR Apache-2.0
-->

# ADR 0001: Mutation testing is a scheduled audit with proven exclusions

## Status

Accepted, 2026-08-01.

## Context

A full mutation run over this suite takes hours across four shards, which
disqualifies it as a pull-request gate. Blanket exclusions would hide real
test gaps, so every exclusion needs an individual equivalence argument.

## Decision

Mutation testing runs as a scheduled audit — the weekly `mutation.yml`
workflow, also dispatchable on demand — and its latest result is reviewed
before a release. Run one shard locally with `just mutants-ci <shard>` (for
example `just mutants-ci 0/4`), with the vendored standalone backend
available via `CONPTY_OXIDE_TEST_DLL_DIR`. The recipe's 90-second timeout
leaves room for nextest to cancel concurrent process tests after an intended
kill-tree failure; shorter budgets misclassify caught mutants as timeouts.

A mutant that changes behaviour must be caught by a focused test. An
exclusion is recorded in `.cargo/mutants.toml` only when the mutated
expression is provably identical for every input, and each regex stays tied
to one file, function, operator, and replacement. Files or classes of
mutations are never excluded.

## Consequences

Pull requests stay fast while every mutation-visible behaviour change is
still audited on a weekly cadence, and the exclusion list cannot silently
grow: each entry must carry a proof below and a matching narrow regex.

## Recorded equivalences

- `Pty::builder` returning `PtyBuilder::default()` versus
  `Default::default()` selects the same unique `Default` implementation.
- The four Tokio `poll_flush` implementations already return the exact
  replacement, `Poll::Ready(Ok(()))` / `Poll::from(Ok(()))`. These streams
  add no userspace buffer, and their direct no-op contract is separately
  tested.
- Replacing bitwise OR with XOR in the external-DLL search flags is
  identical: `0x100` and `0x800` have no common bit.
- Replacing bitwise OR with XOR in the legacy root wait flags is identical:
  `WT_EXECUTEONLYONCE` (`0x8`) and `WT_EXECUTELONGFUNCTION` (`0x10`) have no
  common bit. The exclusion is restricted to the operator at column 32 in
  `spawn_root_watcher_inner`.
- Both named-pipe server access flags and the file-mode flags occupy
  disjoint bits. The byte/read/wait mode constants are zero, so OR and XOR
  also produce the same local-only mode value.
- In `pipe_mode`, `PIPE_TYPE_BYTE`, `PIPE_READMODE_BYTE`, and `PIPE_WAIT`
  are all zero. Replacing the OR at column 20 or 41 with AND therefore still
  leaves the accumulated mode at zero. The exclusions are restricted to
  those two operators; the later AND substitution drops
  `PIPE_REJECT_REMOTE_CLIENTS` and remains tested.
- `EventCounter::enabled` is unreachable through `count_events`: its
  `register_callsite` returns `Interest::always()`, which tells `tracing`
  to enable that callsite without consulting `enabled`.
- `EventCounter::max_level_hint` returning `Some(LevelFilter::TRACE)` and
  returning `None` both permit every `tracing` level.

The tests assert the resulting numeric policies. All other AND
substitutions and dropped flags remain included and must fail.
