<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# conpty-oxide

[![CI](https://github.com/P4suta/conpty-oxide/actions/workflows/ci.yml/badge.svg)](https://github.com/P4suta/conpty-oxide/actions/workflows/ci.yml)
[![CodeQL](https://github.com/P4suta/conpty-oxide/actions/workflows/codeql.yml/badge.svg)](https://github.com/P4suta/conpty-oxide/actions/workflows/codeql.yml)
[![crates.io](https://img.shields.io/crates/v/conpty-oxide.svg)](https://crates.io/crates/conpty-oxide)
[![docs.rs](https://docs.rs/conpty-oxide/badge.svg)](https://docs.rs/conpty-oxide)

A Windows-only foundation for using ConPTY safely and naturally from Rust,
with symmetric blocking and Tokio APIs. Requires Windows 10 version 1809 or
later and Rust 1.75 or later.

## Choose a completion path

The default feature enables the blocking API:

```toml
[dependencies]
conpty-oxide = "0.1"
```

When output is unnecessary, `wait` drains and discards it concurrently:

```rust
use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let status = Command::new("cmd.exe")
        .args(["/d", "/c", "echo", "hello"])
        .spawn()?
        .wait()?;
    assert!(status.success());
    Ok(())
}
```

To retain the raw terminal stream, use `collect_output`:

```rust
use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "echo", "hello"])
        .spawn()?
        .collect_output()?;

    assert!(output.status().success());
    print!("{}", String::from_utf8_lossy(output.as_bytes()));
    Ok(())
}
```

For an interactive terminal, IDE, or custom I/O loop, split the managed
session into independently owned I/O, child, and control handles:

```no_run
use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let parts = Command::new("cmd.exe").spawn()?.into_parts();
    // Move parts.output to a reader thread while the current thread drives
    // parts.input, parts.child, and parts.controller.
    drop(parts);
    Ok(())
}
```

For Tokio, disable default features and enable `tokio`:

```toml
[dependencies.conpty-oxide]
version = "0.1"
default-features = false
features = ["tokio"]

[dependencies.tokio]
version = "1"
features = ["io-util", "macros", "rt-multi-thread"]
```

The same three paths are asynchronous where necessary:

```no_run
use conpty_oxide::tokio::Command;

#[tokio::main]
async fn main() -> conpty_oxide::Result<()> {
    let status = Command::new("cmd.exe")
        .args(["/d", "/c", "exit", "0"])
        .spawn()?
        .wait()
        .await?;
    assert!(status.success());

    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "echo", "hello"])
        .spawn()?
        .collect_output()
        .await?;
    assert!(output.status().success());

    let parts = Command::new("cmd.exe").spawn()?.into_parts();
    // Drive parts.output concurrently with input and child control.
    drop(parts);
    Ok(())
}
```

## Managed-session contract

A managed session is bounded by its root process. After the root status is
saved, conpty-oxide terminates descendants that remain in the session Job and
drains the teardown tail to EOF. The reported status is always the root's real
status. This applies after `into_parts` too; splitting ownership does not detach
the process tree.

`wait` discards output without an output-sized allocation. `collect_output`
retains every unread byte and is therefore unbounded; use `wait` when output is
unneeded or `into_parts` to process it as a stream. `Child::wait` is the lower-
level root-process wait and requires the caller to drain output concurrently.

ConPTY has two important stream semantics:

- Dropping or shutting down input ends the terminal session. It is not a
  portable way to deliver stdin EOF to the child.
- Output is one raw UTF-8/VT byte stream. ConPTY does not expose separate stdout
  and stderr channels, and this crate does not parse terminal sequences.

Microsoft recommends servicing conin and conout concurrently, commonly on
separate threads. A single blocking flow can deadlock: once the output pipe is
full, the console host stops making progress, which can also prevent the child
from reaching the point where the caller expects it to exit. `Session::wait`
and `collect_output` provide the safe combined operations; custom integrations
must preserve the same concurrency. See [Microsoft's ConPTY session guidance](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session).

Dropping an unfinished public `Session` or `Child` terminates the complete Job.
`resize` and supported `clear` operations report a `NotConnected` I/O source
after teardown.

## Scope

ConPTY is useful both for terminal implementations and for running programs
that behave differently unless they believe they have a terminal. Typical
uses include:

- terminal UIs and terminal-emulator backends;
- IDE, debugger, and task-runner terminal integration;
- interactive CLI automation that processes raw VT itself;
- running TTY-dependent tools with resize and process-tree control.

This crate intentionally does not provide a VT parser, terminal widget,
`expect` engine, shell protocol, or cross-platform PTY abstraction. Those are
higher layers with different policy choices. The project follows the same
practical PTY use cases described by
[node-pty](https://github.com/microsoft/node-pty), while keeping its own API Windows-specific and Rust-native.

Cursor inheritance, manual EOF policy, detached sessions, raw `HPCON` access,
and `Command::output`/`status` convenience methods are outside the 0.1 API.
Future advanced operations should be typed around tasks rather than expose
unchecked Win32 flags.

The `blocking`, `tokio`, and `tracing` crate features may be combined. Backend
selection prefers a validated `conpty.dll`/`OpenConsole.exe` pair next to the
application when available, then falls back to the system ConPTY.

## Links

- [API documentation](https://docs.rs/conpty-oxide)
- [Examples](https://github.com/P4suta/conpty-oxide/tree/main/examples)
- [Detailed ConPTY lifecycle notes](https://github.com/P4suta/conpty-oxide/blob/main/docs/conpty-pitfalls.md)
- [Changelog](https://github.com/P4suta/conpty-oxide/blob/main/CHANGELOG.md)
- [Contributing](https://github.com/P4suta/conpty-oxide/blob/main/CONTRIBUTING.md)
- [Security policy](https://github.com/P4suta/conpty-oxide/security/policy)

Licensed under either Apache-2.0 or MIT, at your option.
