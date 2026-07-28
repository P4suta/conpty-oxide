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

Status: 0.1.0. The API is not stable yet; expect breaking changes before 1.0.

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
  orphaning it. `Command::kill_on_drop` also asks the kernel to finish the job
  when the handle closes, so the tree dies even if no destructor ever runs.
- **Sync and async.** A blocking front end (`conpty_oxide::blocking`) and a
  Tokio one (`conpty_oxide::Pty`, `AsyncRead` + `AsyncWrite`, `Child::wait` as
  a future). The two are independent and either can be compiled out.
- **Bundled `conpty.dll` support.** Load the standalone console from a
  directory you name or from next to your executable, with the
  `conpty.dll`/`OpenConsole.exe` version pair validated before any of its code
  runs — a mismatched pair is a hard crash, not a degradation.
- **Capabilities, not guesses.** `ClearPseudoConsole` exists only in the
  standalone DLL and `ReleasePseudoConsole` only on newer builds, so both are
  detected by export and reported (`supports_clear`, `supports_release`);
  asking for a missing one is a typed error, never a surprise.
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
[dependencies]
conpty-oxide = { version = "0.1", default-features = false, features = ["tokio"] }
# The crate itself needs only tokio's net and rt features; these are the ones
# the example below uses directly.
tokio = { version = "1", features = ["io-util", "macros", "rt-multi-thread"] }
```

### Blocking

```rust
use std::io::Read;
use std::thread;

use conpty_oxide::blocking::{Command, Pty};
use conpty_oxide::Size;

fn main() -> conpty_oxide::Result<()> {
    let pty = Pty::builder().size(Size::new(24, 80)).build()?;
    let mut child = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .spawn(&pty)?;

    // The output pipe must be drained while the child runs. The write half is
    // unused here but deliberately kept alive: dropping it would end the
    // session early.
    let (mut reader, writer, _controller) = pty.into_split();
    let output = thread::spawn(move || {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map(|_| buf)
    });

    let status = child.wait()?;
    let output = output.join().expect("the reader thread must not panic")?;
    drop(writer);

    print!("{}", String::from_utf8_lossy(&output));
    assert!(status.success());
    Ok(())
}
```

### Tokio

```rust
use conpty_oxide::{Command, Pty, Size};
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() -> conpty_oxide::Result<()> {
    // Must be built inside a runtime: the session's pipes are registered with
    // its I/O driver.
    let pty = Pty::builder().size(Size::new(24, 80)).build()?;
    let mut child = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .spawn(&pty)?;

    let (mut reader, writer, _controller) = pty.into_split();
    let output = tokio::spawn(async move {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.map(|_| buf)
    });

    let status = child.wait().await?;
    let output = output.await.expect("the reader task must not panic")?;
    drop(writer);

    print!("{}", String::from_utf8_lossy(&output));
    assert!(status.success());
    Ok(())
}
```

Two rules apply to both front ends, and they are the ones ConPTY imposes on
every caller:

1. **Drain the output while the child runs.** The console host writes eagerly;
   a full pipe buffer stops the host, and with it the child. Waiting for the
   child without reading its output deadlocks the session.
2. **Keep the write half alive until the child exits.** Closing the input pipe
   is not "no more input" — the console host reads it as the terminal being
   closed and terminates its clients with `0xC000013A`.

Runnable programs live in [`examples/`](examples): `blocking_echo` is the
first one above, and `tokio_interactive` relays this terminal's input and
output to a real shell running inside a session.

```powershell
cargo run --example blocking_echo
cargo run --features tokio --example tokio_interactive
```

## Using a bundled `conpty.dll`

Sessions run on the operating system's ConPTY unless told otherwise, which
needs no setup at all. Shipping the standalone `conpty.dll` from the
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

```rust
use conpty_oxide::ConPtyBackend;

fn main() -> conpty_oxide::Result<()> {
    // A bundle next to the executable, validated, with the system ConPTY as
    // the fallback. This is also what an unconfigured session uses.
    ConPtyBackend::auto().set_global_default();

    // Or name the directory explicitly and handle a bad bundle yourself.
    let backend = ConPtyBackend::from_dir("vendor/conpty")?;
    println!("{:?}, clear: {}", backend.kind(), backend.supports_clear());

    Ok(())
}
```

`ConPtyBackend::from_dir` reports why a bundle was refused
(`BackendError::DllNotFound`, `OpenConsoleMissing`, `VersionMismatch`,
`MissingExport`); `ConPtyBackend::auto` swallows the same failures and falls
back to the system implementation, logging the rejection under the `tracing`
feature. A backend can be installed process-wide with `set_global_default` or
per session with `PtyBuilder::backend`.

> **The two files must be a matched pair — and a current one.** `conpty.dll`
> launches `OpenConsole.exe` and speaks a private, versioned protocol to it,
> so a mismatched pair does not degrade: it crashes the client process. A
> stale pair crashes too — wezterm#7774 is PowerShell FailFasting under a
> matched but outdated bundle — and version equality cannot detect that, so
> keep the bundle current.
> `from_dir` compares both `ProductVersion` resources and
> refuses a pair it cannot prove consistent — `from_dir_unchecked` opts out of
> that check and should stay unused unless you can guarantee the pair by other
> means. The DLL also only looks for its host next to itself and in the
> native-architecture subdirectory, so putting both files in one directory is
> the layout with no surprises. See
> [pitfalls 4 and 5](docs/conpty-pitfalls.md#a-bundled-dll-and-its-console-host-must-be-a-matched-pair).

## Platform support

Windows only: on any other target the crate stops the build with an
explanatory `compile_error!` rather than failing to link.

| Windows | Behaviour |
| --- | --- |
| Older than 10 1809 (build 17763) | No ConPTY. The binary still loads — the API is resolved with `GetProcAddress` — and building a session fails with `Error::Backend`. |
| 10 1809 through 11 23H2, Server 2019/2022 | Legacy lifecycle: the console host outlives the child, so end-of-file is forced by a watcher thread about a second after the root child exits. |
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
  `blocking` and `asyn` state the lifecycle rules in full.

## Roadmap

- The entry points the standalone `conpty.dll` exports and this crate does not
  surface yet: `ConptyCreatePseudoConsoleAsUser`,
  `ConptyShowHidePseudoConsole`, `ConptyReparentPseudoConsole`.
- Exit detection through `RegisterWaitForSingleObject`, so that waiting on a
  child — and the legacy watcher — no longer parks a thread in
  `WaitForSingleObject`.
- A cross-platform facade, so callers that also target Unix can share code
  while this crate stays the Windows backend.

## Development

`just lint` and `just test` run everything CI checks.

The tests that drive a bundled `conpty.dll` need one to drive, so they are
opt-in:

```powershell
just fetch-conpty  # download the pinned ConPTY package into vendor/conpty
just test-dll      # run the suite with CONPTY_OXIDE_TEST_DLL_DIR set
```

`just fetch-conpty` downloads a pinned version of the
[`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
package, verifies its SHA-256, and lays `conpty.dll` out next to the matching
`OpenConsole.exe`. Nothing from it is committed — `vendor/` is ignored.
Without that directory the external-backend tests note the skip on stderr and
the rest of the suite runs unchanged; CI always fetches it, so both backends
are covered there.

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
