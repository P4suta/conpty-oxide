<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# conpty-oxide

A correctness-first ConPTY (Windows pseudoconsole) library for Rust, with a
blocking API and a Tokio one.

ConPTY is easy to call and hard to survive: the documented teardown call can
block forever, the output pipe never reaches end-of-file on older Windows,
closing the input pipe kills the child instead of signalling it, and a bundled
`conpty.dll` paired with the wrong console host takes the process down. This
crate treats those failure modes as the product — see
[docs/conpty-pitfalls.md](docs/conpty-pitfalls.md) for the full catalogue with
primary sources.

Status: 0.1.0. The 0.1 API is deliberately small and centered on managed
sessions; later minor releases may still make breaking changes before 1.0.

## Why this crate exists

The ecosystem already had good PTY crates; as of this writing, none of them
covered this corner.

- **`portable-pty`** (from wezterm) is cross-platform and synchronous: a
  session hands out `Read`/`Write` objects for you to drive on threads of your
  own.
- **`pty-process`** has both blocking and async front ends, and targets Unix
  ptys — not Windows.
- **`conpty`** is a small, direct wrapper around the Win32 pseudoconsole calls.

What was missing was a Windows-only crate with an async front end that treats
ConPTY's lifecycle as the hard part rather than as plumbing: one that promises
a real end-of-file contract, cannot hang on teardown in any drop order, kills
the whole process tree, and loads a bundled `conpty.dll` only after proving the
bundle is consistent.

## Features

- **End-of-file contract.** Once the session is over, reading the output
  returns `Ok(0)` — after every byte the child wrote, and without the caller
  closing or polling anything. Disconnect-flavoured OS errors
  (`ERROR_BROKEN_PIPE` and friends) are mapped to that same clean end-of-file.
  Old and new Windows reach it by different routes and behave identically.
- **Teardown that cannot hang.** A state machine owns the `HPCON` and runs
  `ClosePseudoConsole` exactly once, only in situations where it is proven not
  to block indefinitely, and never on the thread that reads output. Dropping
  the parts of a session in any order completes.
- **Kill the whole tree.** The child and every descendant join a job object
  assigned at creation time, so `Child::kill` terminates the tree rather than
  orphaning it. Managed sessions also ask the kernel to finish the job when
  ownership is dropped.
- **Sync and async.** Symmetric blocking (`conpty_oxide::blocking`) and Tokio
  (`conpty_oxide::tokio`) front ends. The module path always states which I/O
  model a type uses, and either front end can be compiled out.
- **Bundled `conpty.dll` support.** Load the standalone console from a
  directory you name or from next to your executable, with the
  `conpty.dll`/`OpenConsole.exe` version pair validated before any of its code
  runs — a mismatched pair is a hard crash, not a degradation.
- **Capabilities, not guesses.** `ClearPseudoConsole` exists only in the
  standalone DLL, so availability is detected by export and reported through
  `supports_clear`; asking for a missing operation is a typed error.
- **Optional `tracing`.** Silent fallbacks — an ignored bundle, a demoted
  session — are logged rather than invisible.

## Quick start

```toml
[dependencies]
conpty-oxide = "0.1"
```

The default `blocking` feature gives the synchronous API. For the async one,
add the `tokio` feature (and drop the default if you do not need both):

```toml
[dependencies.conpty-oxide]
version = "0.1"
default-features = false
features = ["tokio"]

[dependencies]
# The crate itself needs only tokio's net and rt features; these are the ones
# the example below uses directly.
tokio = { version = "1", features = ["io-util", "macros", "rt-multi-thread"] }
```

### Blocking

```rust
use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .output()?;

    print!("{}", String::from_utf8_lossy(output.as_bytes()));
    assert!(output.status().success());
    Ok(())
}
```

### Tokio

```rust
use conpty_oxide::tokio::Command;

#[tokio::main]
async fn main() -> conpty_oxide::Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .output()
        .await?;

    print!("{}", String::from_utf8_lossy(output.as_bytes()));
    assert!(output.status().success());
    Ok(())
}
```

`Command::spawn()` returns a managed `Session` instead when input and output
need to remain interactive. Dropping that session, or a `Child` obtained from
`Session::into_parts`, terminates its entire process tree.

### Interactive managed sessions

Use `Session::into_parts` when input, output, process waiting, and terminal
control need to progress independently:

```rust
use std::io::Read;
use std::thread;

use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let parts = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .spawn()?
        .into_parts();
    let mut child = parts.child;
    let mut output = parts.output;
    let input = parts.input;
    let controller = parts.controller;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).map(|_| bytes)
    });

    let status = child.wait()?;
    let bytes = reader.join().expect("reader thread panicked")?;
    drop(input);
    drop(controller);
    assert!(status.success());
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}
```

Two rules apply to interactive sessions, and they are the ones ConPTY imposes on
every caller:

1. **Drain the output while the child runs.** The console host writes eagerly;
   a full pipe buffer stops the host, and with it the child. Waiting for the
   child without reading its output deadlocks the session.
2. **Keep the write half alive until the child exits.** Closing the input pipe
   is not "no more input" — the console host reads it as the terminal being
   closed and terminates its clients with `0xC000013A`.

Cursor inheritance, manual EOF policy, detached sessions, and pre-staged
spawning are intentionally outside the 0.1 API. They may return later as typed
advanced operations when concrete users need them.

Runnable programs live in [`examples/`](examples): `blocking_echo` uses
managed output collection, and `tokio_interactive` relays this terminal's
input and output to a real shell through managed session parts. The examples
and managed quick starts are compiled by the all-features documentation gate.

```powershell
cargo run --example blocking_echo
cargo run --features tokio --example tokio_interactive
```

## Using a bundled `conpty.dll`

Automatic selection needs no setup: it prefers a validated standalone
`conpty.dll` bundle next to the executable, then falls back to the operating
system's ConPTY. Shipping that DLL from the
[`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
package (MIT, published by the microsoft/terminal team) buys two things: the
newer console host's behaviour — including `ReleasePseudoConsole`, and with it
prompt end-of-file — on Windows versions that do not have it natively, and
`ClearPseudoConsole`, which the system API does not expose at all.

Fetch the pinned, checksum-verified bundle into `vendor/conpty`:

```powershell
just fetch-conpty
```

Or take it from NuGet by hand: unpack the `.nupkg` and copy
`runtimes/win-<arch>/native/conpty.dll` and
`build/native/runtimes/<arch>/OpenConsole.exe` into **one** directory. Both
files, from the same package version, are required.

Point the crate at it:

```rust,no_run
use conpty_oxide::blocking::Command;
use conpty_oxide::{ConPtyBackend, SessionOptions};

fn main() -> conpty_oxide::Result<()> {
    // A bundle next to the executable, validated, with the system ConPTY as
    // the fallback. This is also what an unconfigured session uses.
    let automatic = ConPtyBackend::auto()?;

    // Or name the directory explicitly and handle a bad bundle yourself.
    let backend = ConPtyBackend::from_dir("vendor/conpty")?;
    println!("clear supported: {}", backend.supports_clear());

    // Backend choice is explicit per managed session; there is no mutable
    // process-wide default.
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .spawn_with(SessionOptions::new().backend(automatic))?
        .wait_with_output()?;
    assert!(output.status().success());

    Ok(())
}
```

`ConPtyBackend::from_dir` reports why a bundle was refused through
`BackendError::kind` and `BackendErrorKind`. `ConPtyBackend::auto` logs and
ignores a rejected adjacent
bundle, then falls back to the system implementation. It returns `Err` only
when no usable system ConPTY is available either, so every successful result is
a usable backend. Pass one per managed session with `SessionOptions::backend`.

> **The two files must be a matched pair — and a current one.** `conpty.dll`
> launches `OpenConsole.exe` and speaks a private, versioned protocol to it,
> so a mismatched pair does not degrade: it crashes the client process. A
> stale pair crashes too — wezterm#7774 is PowerShell FailFasting under a
> matched but outdated bundle — and version equality cannot detect that, so
> keep the bundle current.
> `from_dir` compares both `ProductVersion` resources and refuses a pair it
> cannot prove consistent. The DLL also only looks for its host next to itself
> and in the native-architecture subdirectory, so putting both files in one
> directory is the layout with no surprises. See
> [pitfalls 4 and 5](docs/conpty-pitfalls.md#a-bundled-dll-and-its-console-host-must-be-a-matched-pair).

## Platform support

Windows only: on any other target the crate stops the build with an
explanatory `compile_error!` rather than failing to link.

| Windows | Behaviour |
| --- | --- |
| Older than 10 1809 (build 17763) | No ConPTY. The binary still loads — the API is resolved with `GetProcAddress` — and building a session fails with `ErrorKind::Backend`. |
| 10 1809 through 11 23H2, Server 2019/2022 | Legacy lifecycle: the console host outlives the child, so a registered wait triggers a short-lived grace/close worker after the root child exits. |
| 11 24H2 / Server 2025 (build 26100) and later | `ReleasePseudoConsole` fast path: the session is released right after the spawn, the host exits with its last client, and end-of-file arrives on its own. |

A current bundled `conpty.dll` gives the fast path on every supported version
of Windows, whatever the operating system's own ConPTY can do. Both
lifecycle modes are exercised in CI (Server 2022 and Server 2025 legs, each
also against the vendored bundle), and the crate's own tests can force the
legacy path on a modern machine so it never goes untested.

The minimum supported Rust version is **1.75**, verified in CI.

## Documentation

- [docs/conpty-pitfalls.md](docs/conpty-pitfalls.md) — the eleven ConPTY
  failure modes this crate is built around: what goes wrong, why, how it is
  handled here, and the primary source for each.
- API documentation: `cargo doc --all-features --open`. The module docs of
  `blocking` and `tokio` state the lifecycle rules in full.

## Roadmap

- The entry points the standalone `conpty.dll` exports and this crate does not
  surface yet: `ConptyCreatePseudoConsoleAsUser`,
  `ConptyShowHidePseudoConsole`, `ConptyReparentPseudoConsole`,
  `ConptyPackPseudoConsole`.
- A cross-platform facade, so callers that also target Unix can share code
  while this crate stays the Windows backend.

## Development

With PowerShell 7, Rustup, mise, and just available, install the exact
toolchain and hooks recorded in `mise.lock`:

```powershell
just setup
```

The hooks are intentionally staged. Pre-commit runs formatting, REUSE,
source-policy, dependency, spelling, Markdown, YAML, workflow, and TOML checks.
Pre-push adds the full Clippy feature powerset, supply-chain checks, PowerShell
analysis, nextest, and doctests. CI additionally covers the MSRV, all supported
Windows architectures, both ConPTY lifecycle modes, the validated DLL bundle,
and the 92% line/region/function coverage floor.

Useful commands:

```powershell
just lint          # every static policy and Clippy configuration
just test          # nextest plus doctests
just coverage      # DLL-backed LLVM coverage with enforced thresholds
just mutants-list  # inspect mutation candidates without editing source
just mutants       # full mutation suite in a safe temporary copy
just ci            # complete required-CI equivalent, except scheduled mutation
```

`just fetch-conpty` downloads a pinned version of the
[`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
package, verifies its SHA-256, and lays `conpty.dll` out next to the matching
`OpenConsole.exe`. Nothing from it is committed — `vendor/` is ignored.
Without that directory the external-backend tests note the skip on stderr and
the rest of the suite runs unchanged; CI always fetches it, so both backends
are covered there.

Every project-owned file follows REUSE 3.3. New commentable files must carry
the repository SPDX copyright and `MIT OR Apache-2.0` identifier; generated or
non-commentable files belong in `REUSE.toml`. `reuse lint` is a required local
and CI gate. Rust source also has a repository policy of no dynamic trait
objects, no ignored tests, and no lint suppressions except the documented
shared integration-test harness exception.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
