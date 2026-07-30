<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# ConPTY pitfalls

Windows' pseudoconsole API is small — a handful of entry points and two pipes
— and has a remarkable number of ways to hang the calling process, lose the
output you were collecting, or kill the child you were talking to. Few of them
produce an error code: they produce a program that stops, or a crash a long way
from the call that caused it.

This is the catalogue [`conpty-oxide`](../README.md) was built from: eleven
failure modes, why each one exists, and what the crate does about it.

## How to read this

Every entry has the same four parts:

- **What happens** — the observable symptom.
- **Why** — the architectural reason, so the workaround is not cargo cult.
- **In this crate** — how `conpty-oxide` handles it, with the source file.
- **Sources** — primary documentation and issue reports.

Statements marked **Measured here** were verified by running code in this
repository rather than read somewhere, and name the test that keeps them true;
statements marked **Found here** come from comparing this crate's
implementation against a primary source and finding a divergence. The
measurements were taken on Windows 11 build 26200 and in CI, which runs the
suite on Windows Server 2022 (no `ReleasePseudoConsole`) and Windows Server
2025 (build 26100, with it), each leg additionally against the vendored
standalone `conpty.dll` (`Microsoft.Windows.Console.ConPTY` 1.24.260710001).
Nothing here is extrapolated from one machine or one lifecycle mode.

Two terms are used throughout: **conout** is the pipe the console host writes
rendered output to, **conin** the pipe it reads input from. The *console host*
is the `conhost.exe` or `OpenConsole.exe` process that `CreatePseudoConsole`
starts on your behalf.

## Contents

1. [Closing a pseudoconsole can block forever](#closing-a-pseudoconsole-can-block-forever)
2. [Releasing the pseudoconsole is what ends the output stream](#releasing-the-pseudoconsole-is-what-ends-the-output-stream)
3. [Closing the input pipe closes the terminal, not stdin](#closing-the-input-pipe-closes-the-terminal-not-stdin)
4. [A bundled DLL and its console host must be a matched pair](#a-bundled-dll-and-its-console-host-must-be-a-matched-pair)
5. [Where a bundled DLL looks for its console host](#where-a-bundled-dll-looks-for-its-console-host)
6. [Inheriting the cursor stops input until the reply arrives](#inheriting-the-cursor-stops-input-until-the-reply-arrives)
7. [Exit detection and killing the process tree](#exit-detection-and-killing-the-process-tree)
8. [What Tokio and mio do with a dropped pipe](#what-tokio-and-mio-do-with-a-dropped-pipe)
9. [ConPTY needs synchronous handles](#conpty-needs-synchronous-handles)
10. [The legacy grace period is latency, not correctness](#the-legacy-grace-period-is-latency-not-correctness)
11. [The output is a UTF-8 virtual terminal stream](#the-output-is-a-utf-8-virtual-terminal-stream)

## Closing a pseudoconsole can block forever

**What happens.** A program spawns a child into a pseudoconsole, stops reading
conout (or never starts), and then tears the session down. `ClosePseudoConsole`
does not return. There is no error, no timeout, and no diagnostic — the thread
is simply gone. Doing the same from the thread that reads conout deadlocks even
when everything else is right.

**Why.** Closing a pseudoconsole asks the console host to exit and waits for it
to finish. The host still has rendered output queued for conout, and while
somebody holds the read end open, its writes block on a full pipe buffer. The
call is therefore waiting for progress only the reader can make — and if the
caller *is* the reader, nobody can make it. Microsoft's own guidance is to
close the output pipe first or keep draining it while the close runs. On builds
that also export `ReleasePseudoConsole` (Windows 11 24H2 / Server 2025, build
26100 and later, and the standalone `conpty.dll`), the close no longer waits
for a reader at all — which is what makes the released lifecycle in
[pitfall 2](#releasing-the-pseudoconsole-is-what-ends-the-output-stream)
possible.

**Measured here.** `tests/close_hang.rs` and `tests/tokio_close_hang.rs` drive a
child that writes roughly 280 KiB (4000 lines) into a session nobody reads,
confirm the pipe buffer really did fill (the child must still be running), and
then destroy the session in each of four drop orders. Every order has to finish
within five seconds. The CI matrix runs the public drop-order suite on both
pre-26100 and current Windows, while crate-internal tests strip the release
export to exercise legacy transitions deterministically on newer machines.

**In this crate.** `src/core/pseudocon.rs` holds a small state machine over
(reader state, close state). It guarantees that `ClosePseudoConsole` runs
exactly once, and it enumerates the five situations in which the call is
allowed to run, each with an argument for why it cannot block indefinitely:
after the reader saw end-of-file, after the reader's handle is retired, an
explicit request with no live reader, an explicit request from the legacy
post-exit close worker (which is allowed to block), and an explicit request in
released mode (deferred to the reader's own transition). The reader thread
only ever runs the close after end-of-file, which proves the host is already
gone. The final defence is `Drop`, which never blocks: where promptness cannot
be proven, the `HPCON` goes to a detached thread instead. The public
consequence is the one stated in the API docs — dropping the parts of a
session in any order completes.

**Sources.**

- [`ClosePseudoConsole`](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole)
- [Creating a pseudoconsole session](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session)
- [Introducing the Windows Pseudo Console (ConPTY)](https://devblogs.microsoft.com/commandline/windows-command-line-introducing-the-windows-pseudo-console-conpty/)

## Releasing the pseudoconsole is what ends the output stream

**What happens.** On Windows versions before 24H2, a reader that waits for
end-of-file on conout after the child has exited waits forever. The child is
gone, the exit status is available, and the pipe stays open.

**Why.** The console host's lifetime is tied to the `HPCON`, not to its
clients. Until the owner closes the pseudoconsole, the host stays alive holding
the write end of conout, so the read end never breaks. `ReleasePseudoConsole`
inverts that: it hands the host its own lifetime back, and the host exits once
its last client disconnects. Conout then breaks with `ERROR_BROKEN_PIPE` and
end-of-file arrives on its own — the model node-pty adopted. Two details are
easy to get wrong. Releasing does **not** free the `HPCON`;
`ClosePseudoConsole` still has to run afterwards. And the capability must be
detected by looking for the export with `GetProcAddress`, not by comparing
build numbers, because compatibility shims and backported builds make version
checks misfire (microsoft/terminal#19112).

**In this crate.** `src/core/session.rs` performs exactly three steps per
spawn, in this order: create the job object and the process, call
`ReleasePseudoConsole`, and — only if that did not happen — start the legacy
watcher. Releasing before a client exists would release a console nobody is
attached to; skipping the watcher on a legacy backend would leave a reader
waiting for an end-of-file that can never arrive. `ConPtyBackend` resolves
every entry point with `GetProcAddress` and reports
the backend's internal release capability check; a failed release is logged
(with the `tracing` feature) and demotes that session to the legacy path
rather than failing it.

**Sources.**

- [microsoft/terminal#19112](https://github.com/microsoft/terminal/issues/19112)
  — detect `ReleasePseudoConsole` by export, not by build number
- [node-pty's ConPTY implementation](https://github.com/microsoft/node-pty/blob/main/src/win/conpty.cc)

## Closing the input pipe closes the terminal, not stdin

**What happens.** A program finishes writing input and closes the write end of
conin, the way it would close a child's stdin. The child dies immediately with
exit code `0xC000013A` (`STATUS_CONTROL_C_EXIT`), and output it had produced
but not yet flushed is lost.

**Why.** Conin is not the child's stdin; it is the terminal's keyboard. The
console host reads end-of-file there as "the terminal window went away" and
does what a closing console does: it sends a close event to every attached
client, and clients that do not handle it are terminated with
`STATUS_CONTROL_C_EXIT`. There is no in-band way to say "no more input" —
whatever the child uses as an end-of-input marker (`^Z` for many Windows
console programs) has to be written as data.

**Measured here.** Every session helper in `tests/helpers` keeps the write half
alive for the child's whole life, and says why: dropping it early terminated
the child with `0xC000013A` and truncated its output, which would have made the
end-of-file tests pass for the wrong reason.

**In this crate.** The write half is a first-class part of the session
(`OwnedWriteHalf`) whose documentation states that dropping it *ends* the
session rather than signalling one, and the same is true of
`AsyncWriteExt::shutdown` on the async front end. Nothing inside the crate
closes conin early: the shutdown paths retire conout first. Both examples in
`examples/` hold the write half until the child has exited — the async one
parks a task on it rather than letting it drop when stdin ends.

**Sources.**

- [`HandlerRoutine`](https://learn.microsoft.com/en-us/windows/console/handlerroutine)
  — `CTRL_CLOSE_EVENT` and what happens to clients that do not handle it
- [MS-ERREF NTSTATUS values](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-erref/596a1078-e883-4972-9bbc-49e60bebca55)
  — `STATUS_CONTROL_C_EXIT` is `0xC000013A`

## A bundled DLL and its console host must be a matched pair

**What happens.** An application ships the standalone `conpty.dll` with an
`OpenConsole.exe` from a different release. Sessions do not degrade — the
client process dies later at an unrelated point, with no error anywhere near
the two files that caused it.

**Why.** `conpty.dll` does not host the console itself; it launches
`OpenConsole.exe` and talks to it over a private, versioned protocol
(`winconpty.cpp` in microsoft/terminal). The two files are shipped as a pair
for that reason, and nothing at load time notices that they disagree.

The in-the-wild report usually cited here, wezterm#7774, is worth quoting
accurately, because it is about the *adjacent* failure: wezterm shipped a
**matched but outdated** pair (both files `1.22.2502.04002`), PowerShell died
with a `0x8013_1623` FailFast on exiting a TUI app, and the fix was updating
both files together to `1.24.260402001`. Two lessons follow. A stale ConPTY
bundle crashes clients even when it is internally consistent — a
version-equality check is silent about that, so keeping the bundle current is
the application's job. And torn pairs are easy to produce by accident: the
Windows Terminal MSIX ships only `OpenConsole.exe`, while `conpty.dll` comes
from the separate NuGet package, so a partial update splits the pair.

**Version strings are wider than 16 bits.** The natural way to compare the
pair is the `ProductVersion` resource of both files, and the natural way to
parse that is the way `VS_FIXEDFILEINFO` stores versions: four `u16` fields.
That is wrong here. The ConPTY packages stamp a nine-digit date serial into the
build component — the vendored package reports `1.24.260710001` — which
overflows a 16-bit parse. A parser that stops at the overflowing component
silently degrades the comparison to major.minor, and major.minor is equal for
exactly the mismatched pairs the check exists to reject.

**Measured here.** Two real releases of the same minor line —
`1.24.260710001` and `1.24.260303001` — were compared with both parsers. The
16-bit parse accepted the cross-release pair; the 64-bit one refuses it with
`BackendErrorKind::VersionMismatch`, which is why the loader parses `u64`. The
tests `parse_version_reads_the_numeric_prefix` and
`version_pair_compatibility` in `src/backend.rs` keep both halves true: the
former pins the nine-digit component parse, the latter pins the refusal of
exactly this pair.

**In this crate.** `ConPtyBackend::from_dir` validates a bundle before any of
its code runs: the DLL must exist and be a regular file, the `OpenConsole.exe`
the DLL will actually launch must be found, both `ProductVersion` resources
must be readable and equal, and only then is the DLL mapped — by absolute path,
with `LoadLibraryExW` search flags that consult neither `PATH` nor the current
directory nor the registry. Version components are parsed as `u64`, and a
version that cannot be read counts as a mismatch rather than as "probably
fine". `ConPtyBackend::from_dir_unchecked` exists for the case where the
version resources are unreadable for a reason the caller controls, and its
documentation says plainly what it is opting out of. `ConPtyBackend::auto`
applies the same validation to the executable's own directory and falls back to
the system ConPTY, logging the rejection under the `tracing` feature so a
bundle that is silently ignored is still diagnosable. It returns an error only
if the system ConPTY is unavailable too.

**Sources.**

- [wezterm#7774](https://github.com/wez/wezterm/issues/7774) — a matched but
  outdated pair FailFasts PowerShell (`0x8013_1623`), and the MSIX/NuGet
  packaging split that makes torn pairs likely
- [`winconpty.cpp`](https://github.com/microsoft/terminal/blob/main/src/winconpty/winconpty.cpp)
  — the DLL half of the private protocol, and the launch of `OpenConsole.exe`
- [`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
  — the official MIT-licensed distribution of the pair
- [`VS_FIXEDFILEINFO`](https://learn.microsoft.com/en-us/windows/win32/api/verrsrc/ns-verrsrc-vs_fixedfileinfo)
  — the 16-bit binary fields the free-form string is not bound by

## Where a bundled DLL looks for its console host

**What happens.** A bundle is laid out with `conpty.dll` at the top and
`OpenConsole.exe` in an architecture subdirectory, a validator finds the pair,
declares it consistent — and every session still runs against the operating
system's inbox `conhost.exe`. The bundle was validated but never used, so
whatever the newer host was supposed to fix is still broken.

**Why.** `conpty.dll` searches for its console host in a specific order: next
to itself first, then in the *single* subdirectory named after the machine's
native architecture, then it falls back to the inbox `conhost.exe`. The
architecture comes from `IsWow64Process2`'s native machine, not from the
architecture of the running process, so an emulated process (an x64 build on
ARM64 Windows) must reach the same answer the DLL will. Subdirectories for any
other architecture are never searched.

**Found here.** This crate's loader originally accepted an `OpenConsole.exe` in
any of the three architecture subdirectories. Comparing that against
`winconpty`'s own `_ConsoleHostPath` showed the divergence — a bundle it
approved could still run every session against the inbox host — and commit
`90e3094` narrowed it to the two locations the DLL really searches.

**In this crate.** `find_console_host` in `src/backend/bundle.rs` mirrors the
DLL's search exactly: adjacent first, then the native-architecture
subdirectory resolved through a dynamically loaded `IsWow64Process2`. The inbox
`conhost.exe` is deliberately not accepted as a validation target — falling
back to it hands the caller a backend with the behaviour they were trying to
replace. `scripts/fetch-conpty.ps1` lays both files out side by side, which is
the layout with no ambiguity at all.

**Sources.**

- [`winconpty.cpp`](https://github.com/microsoft/terminal/blob/main/src/winconpty/winconpty.cpp)
  — `_ConsoleHostPath` and the fallback to the inbox `conhost.exe`
- [`IsWow64Process2`](https://learn.microsoft.com/en-us/windows/win32/api/wow64apiset/nf-wow64apiset-iswow64process2)

## Inheriting the cursor stops input until the reply arrives

**What happens.** A session created with `PSEUDOCONSOLE_INHERIT_CURSOR` accepts
no input at all: everything written to conin is ignored, and the child sits
there as if the keyboard were unplugged. Teardown hangs have been reported with
the flag set as well.

**Why.** The flag makes the new pseudoconsole ask the *outer* terminal where
its cursor is, by writing a Device Status Report (`ESC [ 6 n`) to conout
immediately after creation. Input processing is suspended until the reply comes
back on conin. A caller that is not already draining conout and echoing the
reply therefore deadlocks the input direction by construction — and this is a
creation-time flag, so the deadlock is armed before the caller has done
anything else.

**In this crate.** The flag is off by default in both front ends.
`PtyBuilder::inherit_cursor` exists, and its documentation states the two
conditions under which it is usable (output already drained by another
thread or task, and the reply echoed back) along with the outstanding hang
report.

**Sources.**

- [`CreatePseudoConsole`](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole)
  — `PSEUDOCONSOLE_INHERIT_CURSOR`
- [microsoft/terminal#17688](https://github.com/microsoft/terminal/issues/17688)
  — teardown hangs reported with the flag set

## Exit detection and killing the process tree

**What happens.** Three separate mistakes converge here. Waiting for conout to
reach end-of-file as a way of detecting that the child exited never fires on a
legacy backend. Calling `GetExitCodeProcess` without waiting first "succeeds"
and reports `STILL_ACTIVE` (259), which is indistinguishable from a child that
exited with 259. And terminating the process this crate spawned leaves its
descendants running — the shell dies, the build tool it started keeps holding
files open.

**Why.** End-of-file is a property of the console host's lifetime, not the
child's (see [pitfall 2](#releasing-the-pseudoconsole-is-what-ends-the-output-stream)),
so it is the wrong signal to derive exit from. `STILL_ACTIVE` is a documented
sentinel value that shares the `u32` space with real exit codes, so only
sequencing — wait first, read the code afterwards — distinguishes them. And a
process handle owns one process; the tree needs a job object.

**Measured here.** `tests/kill_tree.rs` starts `cmd.exe`, has it start a
`ping -t` grandchild, confirms the grandchild appears in the system process
list, kills the session, and requires the grandchild to disappear. It runs in
both lifecycle modes.

**In this crate.** `ProcessWaiter` in `src/core/wait.rs` implements blocking
`wait` and the shared zero-timeout `try_wait`. Tokio `Child::wait` duplicates
the process handle into a one-shot Windows registered wait instead, so no
runtime or crate thread is parked while the child lives. Both paths read the
exit code only after Windows has signalled the process. The job object in
`src/core/job.rs` is created before the process and attached with
`PROC_THREAD_ATTRIBUTE_JOB_LIST`, so the child joins the job before its first
instruction — no `CREATE_SUSPENDED`/`AssignProcessToJobObject`/`ResumeThread`
dance, and no window in which a grandchild could escape. `Child::kill`
terminates the job, not the process. Note that the console host is *not* in the
job: it is a child of the calling process created by `CreatePseudoConsole`, so
killing the tree ends the session's programs and leaves the pseudoconsole to
the lifecycle state machine, exactly as an ordinary exit would.

**Sources.**

- [`GetExitCodeProcess`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess)
  — the `STILL_ACTIVE` caveat
- [`UpdateProcThreadAttribute`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute)
  — `PROC_THREAD_ATTRIBUTE_JOB_LIST`
- [Job objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)

## What Tokio and mio do with a dropped pipe

**What happens.** An async session is torn down; the code drops the pipe halves
and then closes the pseudoconsole, reasoning that the conout read end is gone
and the close is therefore safe. Against a legacy console host wedged writing
into a full conout buffer, that close blocks — on a current-thread runtime
permanently, because the thread it blocks is the only one that could have
retired the cancelled read and actually closed the handle. Separately, a conin
write that is still pending can hold the input pipe open long past the point
where the caller believed it was closed.

**Why.** Dropping a Tokio `NamedPipeServer` does not close the handle
synchronously. mio issues `CancelIoEx` and lets the I/O driver call
`CloseHandle` when it retires the cancelled operation — which happens the next
time the driver runs, not in the destructor. Between registration and the
driver's next poll, a session's conout read end is therefore still open at the
OS level even though the Rust value is gone. A close that needs "the reader is
gone" as its proof of promptness does not have that proof yet. Pending writes
are the same story from the other side: mio deliberately lets a pending write
finish before closing, which is right for a data pipe and wrong for conin,
where a write goes pending exactly when the console host has stopped reading
and the close *is* the signal the caller is trying to send.

**Found here.** The behaviour was read out of mio 1.2.2's Windows named-pipe
implementation and matched against this crate's async teardown, where two paths
were leaning on the opposite assumption. Commit `fdc3529` fixes them and adds
regression cases that drop a session while the eagerly scheduled conout read is
still in flight — cases whose failure mode is only observable on a pre-24H2
host, which is why they run in CI's Server 2022 leg and behind the
release-stripped backend.

**In this crate.** Async sessions are marked at build time as having a
*deferred* reader close, and the lifecycle state machine refuses to use "the
reader handle is gone" as a promptness proof for them: such a close is handed
to a detached thread instead of running on a runtime worker. The conin writer
issues `CancelIoEx` before dropping its handle, in both `Drop` and
`poll_shutdown`, so a wedged write cannot postpone the close indefinitely. Both
operations are synchronous and non-blocking, which is what makes them legal in
a destructor at all.

**Sources.**

- [mio 1.2.2 `src/sys/windows/named_pipe.rs`](https://github.com/tokio-rs/mio/blob/v1.2.2/src/sys/windows/named_pipe.rs)
- [`CancelIoEx`](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelioex)

## ConPTY needs synchronous handles

**What happens.** An async implementation creates overlapped pipes, hands them
straight to `CreatePseudoConsole`, and gets a session that works on a current
Windows build and misbehaves on an older one.

**Why.** The console host performs plain synchronous reads and writes on the
handles it is given; only recent `OpenConsole` builds cope with handles opened
for overlapped I/O. Anonymous pipes from `CreatePipe` are always synchronous,
which is why the classic sample works everywhere — but they cannot be
registered with an I/O completion port, so an async front end cannot use them
for its own ends.

**In this crate.** The `src/core/pipes.rs` facade selects one feature-local
implementation. `src/core/pipes/anonymous.rs` uses synchronous `CreatePipe`
pairs for the blocking front end. `src/core/pipes/overlapped.rs` builds each
Tokio direction as a single-instance named pipe: the crate keeps the *server*
end, opened with `FILE_FLAG_OVERLAPPED` and registered with Tokio's I/O driver,
and hands `CreatePseudoConsole` the *client* end, opened synchronously. New and
old hosts both see the handle shape they expect. Two details from the
named-pipe API are worth repeating because they are easy to get wrong:
`ConnectNamedPipe` on an overlapped handle must not be passed a null
`OVERLAPPED`, and when the client connected before the call was issued it
fails with `ERROR_PIPE_CONNECTED`, which means success. Every handle is
created non-inheritable as well — a leaked copy of the conout write end would
keep end-of-file from ever arriving.

**Sources.**

- [`CreatePseudoConsole`](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole)
- [`ConnectNamedPipe`](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-connectnamedpipe)
  — the null-`OVERLAPPED` rule and `ERROR_PIPE_CONNECTED`

## The legacy grace period is latency, not correctness

**What happens.** On a backend without `ReleasePseudoConsole`, something has to
force end-of-file after the child exits, and the obvious worry is that closing
the pseudoconsole while the reader is behind truncates the tail of the output.

**Why it is not a correctness problem.** A legacy `ClosePseudoConsole` blocks
until the console host has finished writing, and the reader keeps draining
while it blocks — that is the same property that makes the call dangerous in
[pitfall 1](#closing-a-pseudoconsole-can-block-forever) and harmless here. The
close is lossless. What a grace period buys is latency: in the common case the
reader catches up within it, so teardown begins with nothing left to drain
instead of after a blocking close.

**Measured here.** `tests/eof_semantics.rs` and `tests/tokio_eof.rs` run a
child that prints fifteen uniquely marked lines and assert that every marker
survives the shutdown, in both lifecycle modes — a missing tail shows up as a
specific missing line rather than as a shorter blob. The one-second grace is
therefore a tuning constant, not a load-bearing one; the cost it imposes is
that a legacy session's end-of-file arrives about a second after the child
exits.

**In this crate.** The watcher lives in `src/core/wait.rs` and is armed only
when the session could not be released and `PtyBuilder::eof_on_root_exit` is
set (the default). Windows waits on its own duplicate of the process handle;
the crate creates no thread while the child is alive. After exit, a short-lived
worker sleeps out the grace period and requests the close — never from the
reader's thread. If worker creation fails, the registered long-function
callback completes that post-exit work itself. The builder documents the two
side effects honestly: output from descendants that outlive the root child may
be cut off, and the session is torn down even if the caller still holds the
controller, so `resize` starts failing with `NotConnected`. Turning the watcher
off is supported and leaves the caller responsible for knowing when the
session is finished.

**Sources.**

- [`ClosePseudoConsole`](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole)

## The output is a UTF-8 virtual terminal stream

**What happens.** Code that decodes each read as a standalone string produces
replacement characters at chunk boundaries, and code that expects a resize to
be silent is surprised by a burst of output that repeats what was already on
screen.

**Why.** Conout carries what a terminal emulator would receive: UTF-8 text
interleaved with virtual terminal sequences. The console host writes whenever
it has something to render, so a chunk boundary can fall in the middle of a
multi-byte character or in the middle of an escape sequence. A resize is not a
metadata change either — the host repaints, so `ResizePseudoConsole` produces a
re-emission of the current screen on conout.

**In this crate.** The read halves are byte streams (`Read` / `AsyncRead`) and
their documentation says to decode across reads rather than per read; the crate
does no decoding of its own and hands through exactly what the host wrote.
`Size` is `(rows, cols)` while ConPTY's `COORD` is `(X = columns, Y = rows)`,
so `tests/resize.rs` asks the child what size it thinks it has (`mode con`)
rather than trusting that the call succeeded — a swapped pair succeeds too.

**Sources.**

- [Console virtual terminal sequences](https://learn.microsoft.com/en-us/windows/console/console-virtual-terminal-sequences)
- [`ResizePseudoConsole`](https://learn.microsoft.com/en-us/windows/console/resizepseudoconsole)

## Where each pitfall lives in the source

| Pitfall | Source |
| --- | --- |
| Close hangs, drop order, the five close situations | `src/core/pseudocon.rs` |
| Release after spawn, the three-step spawn order | `src/core/session.rs` |
| Conin is the terminal, not stdin | `src/blocking/pty.rs`, `src/tokio/pty.rs` |
| Bundle validation and version pairs | `src/backend/bundle.rs` |
| Export detection and module pinning | `src/backend/exports.rs` |
| Console host discovery | `src/backend/bundle.rs`, `scripts/fetch-conpty.ps1` |
| Cursor inheritance | `src/backend.rs`, both frontend `builder.rs` modules |
| Exit detection, legacy watcher | `src/core/wait.rs` |
| Kill tree | `src/core/job.rs` |
| Async teardown, cancelled I/O | `src/tokio/pty.rs` |
| Anonymous and overlapped pipe creation | `src/core/pipes/` |

These paths are implementation anchors; the public contracts remain in the
API documentation next to the operations they constrain.
