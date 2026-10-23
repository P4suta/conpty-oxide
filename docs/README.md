<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Conpty-oxide guide

[Return to the project overview](../README.md).

`conpty-oxide` runs Windows ConPTY sessions.
Choose the blocking or Tokio API.

It requires Windows 10 version 1809 or later and Rust 1.75 or later.

## Blocking API

The default `blocking` feature enables the synchronous API.

Add the crate to a Rust project's dependencies with `cargo add conpty-oxide`.

Run a process and wait for its exit status.
The library drains and discards output concurrently.

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

Use `collect_output` to capture the raw terminal output instead.

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

Use `into_parts` for interactive sessions.
Input, output, child, and control handles each have their own owner.

```rust,no_run
use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let parts = Command::new("cmd.exe").spawn()?.into_parts();
    // Move parts.output to a reader thread.
    // Drive the other handles from the current thread.
    drop(parts);
    Ok(())
}
```

## Tokio API

Set `default-features = false` and enable `tokio` for the asynchronous API.

```toml
[dependencies.conpty-oxide]
version = "0.1"
default-features = false
features = ["tokio"]

[dependencies.tokio]
version = "1"
features = ["io-util", "macros", "rt-multi-thread"]
```

The asynchronous API mirrors the blocking one.

```rust,no_run
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

## Session behavior

A session follows its root process's lifetime.

The session terminates remaining descendants when the root exits.
The session reports the root's real exit status.

This also holds after `into_parts`.

Dropping an unfinished `Session` or `Child` terminates the whole Job.

`wait` discards output with bounded memory.

`collect_output` buffers every unread byte.

`Child::wait` waits only for the root process.
The caller must drain output concurrently.

Service ConPTY input and output concurrently.
A full output pipe stalls the session.

`wait` and `collect_output` handle concurrent draining internally.

Output is one raw UTF-8/VT byte stream.
Standard output and standard error share that stream.

Dropping or shutting down input ends the terminal session.
It doesn't send an end-of-file signal to the child's standard input.

Session creation prefers a validated local ConPTY bundle.
It looks for `conpty.dll` and `OpenConsole.exe` next to the executable.
Otherwise, it uses the system ConPTY.

See [Microsoft's ConPTY guidance][conpty-guide] for pipe requirements.

[conpty-guide]: https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session

## Features and non-goals

- `blocking` is the default synchronous API.
- `tokio` provides the asynchronous API on Tokio.
- `tracing` adds instrumentation through the `tracing` crate.

The features work together.

These features are outside this crate's scope:

- VT parsing and terminal widgets
- `expect`-style automation and shell protocols
- Cross-platform pseudoterminal abstraction

This crate provides the Windows session layer for such tools.

## Links

- [API documentation](https://docs.rs/conpty-oxide)
- [Examples](../examples/)
- [ConPTY lifecycle notes](conpty-pitfalls.md)
- [Changelog](../CHANGELOG.md)
- [Support](../SUPPORT.md)
- [Contributing](../CONTRIBUTING.md)
- [Security policy](../SECURITY.md)
- [Code of conduct](../CODE_OF_CONDUCT.md)
