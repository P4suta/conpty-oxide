<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# conpty-oxide

[![CI](https://github.com/P4suta/conpty-oxide/actions/workflows/ci.yml/badge.svg)](https://github.com/P4suta/conpty-oxide/actions/workflows/ci.yml)
[![CodeQL](https://github.com/P4suta/conpty-oxide/actions/workflows/codeql.yml/badge.svg)](https://github.com/P4suta/conpty-oxide/actions/workflows/codeql.yml)
[![crates.io](https://img.shields.io/crates/v/conpty-oxide.svg)](https://crates.io/crates/conpty-oxide)
[![docs.rs](https://docs.rs/conpty-oxide/badge.svg)](https://docs.rs/conpty-oxide)

Windows pseudoconsole (ConPTY) sessions for Rust: spawn a process under a
pseudoconsole, control it, and read its terminal output, with equivalent
blocking and Tokio APIs. Windows-only; requires Windows 10 version 1809 or
later and Rust 1.75 or later.

## Usage

The default `blocking` feature enables the synchronous API:

```toml
[dependencies]
conpty-oxide = "0.1"
```

Run a process and wait for its exit status; output is drained and discarded
concurrently:

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

Capture the raw terminal output instead:

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

For interactive use, split the session into independently owned input,
output, child, and control handles:

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

### Tokio

Disable default features and enable `tokio`:

```toml
[dependencies.conpty-oxide]
version = "0.1"
default-features = false
features = ["tokio"]

[dependencies.tokio]
version = "1"
features = ["io-util", "macros", "rt-multi-thread"]
```

The asynchronous API mirrors the blocking one:

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
    Ok(())
}
```

## Behavior

- A session is bounded by its root process. Once the root exits, descendants
  remaining in the session Job are terminated, and the reported status is the
  root's real exit status. This also holds after `into_parts`.
- Dropping an unfinished `Session` or `Child` terminates the whole Job.
- `wait` discards output with bounded memory. `collect_output` buffers every
  unread byte. The lower-level `Child::wait` waits on the root alone and
  requires the caller to drain output concurrently.
- ConPTY input and output must be serviced concurrently; a full output pipe
  stalls the session (see [Microsoft's ConPTY guidance][ms-conpty]). `wait`
  and `collect_output` handle this internally.
- Output is a single raw UTF-8/VT byte stream. ConPTY has no separate stdout
  and stderr channels.
- Dropping or shutting down input ends the terminal session; it is not a way
  to deliver stdin EOF to the child.
- Backend selection prefers a validated `conpty.dll`/`OpenConsole.exe` pair
  next to the executable and falls back to the system ConPTY.

[ms-conpty]: https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session

## Feature flags

- `blocking` (default) — synchronous API.
- `tokio` — asynchronous API on Tokio.
- `tracing` — instrumentation through the `tracing` crate.

The features can be combined.

## Non-goals

VT parsing, terminal widgets, `expect`-style automation, shell protocols, and
cross-platform PTY abstraction are out of scope. This crate is the
Windows-specific session layer that such tools can build on.

## Links

- [API documentation](https://docs.rs/conpty-oxide)
- [Examples](https://github.com/P4suta/conpty-oxide/tree/main/examples)
- [ConPTY lifecycle notes](https://github.com/P4suta/conpty-oxide/blob/main/docs/conpty-pitfalls.md)
- [Changelog](https://github.com/P4suta/conpty-oxide/blob/main/CHANGELOG.md)
- [Contributing](https://github.com/P4suta/conpty-oxide/blob/main/CONTRIBUTING.md)
- [Security policy](https://github.com/P4suta/conpty-oxide/security/policy)

Licensed under either Apache-2.0 or MIT, at your option.
