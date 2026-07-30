<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# conpty-oxide

[![CI](https://github.com/P4suta/conpty-oxide/actions/workflows/ci.yml/badge.svg)](https://github.com/P4suta/conpty-oxide/actions/workflows/ci.yml)
[![CodeQL](https://github.com/P4suta/conpty-oxide/actions/workflows/codeql.yml/badge.svg)](https://github.com/P4suta/conpty-oxide/actions/workflows/codeql.yml)
[![crates.io](https://img.shields.io/crates/v/conpty-oxide.svg)](https://crates.io/crates/conpty-oxide)
[![docs.rs](https://docs.rs/conpty-oxide/badge.svg)](https://docs.rs/conpty-oxide)

Windows ConPTY sessions for Rust, with blocking and Tokio APIs. Requires
Windows 10 version 1809 or later and Rust 1.75 or later.

## Usage

The default feature enables the blocking API:

```toml
[dependencies]
conpty-oxide = "0.1"
```

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

For Tokio:

```toml
[dependencies.conpty-oxide]
version = "0.1"
default-features = false
features = ["tokio"]

[dependencies.tokio]
version = "1"
features = ["io-util", "macros", "rt-multi-thread"]
```

```rust
use conpty_oxide::tokio::Command;

#[tokio::main]
async fn main() -> conpty_oxide::Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "echo", "hello"])
        .spawn()?
        .collect_output()
        .await?;

    assert!(output.status().success());
    print!("{}", String::from_utf8_lossy(output.as_bytes()));
    Ok(())
}
```

`collect_output` is root-bounded: it drains output while waiting for the root
process, then terminates descendants that outlive it and returns the root exit
status. The program must exit by itself or through its application protocol.
ConPTY has no ordinary stdin half-close; closing terminal input tears down the
session instead of delivering a portable EOF.

Use `Command::spawn()` and `Session::into_parts()` for interactive or custom
I/O coordination. Managed sessions own a kill-on-close Job, so dropping an
unfinished session terminates its entire process tree. ConPTY output is one VT
byte stream rather than separate stdout and stderr streams.

The `blocking`, `tokio`, and `tracing` crate features may be combined. Backend
selection uses a validated `conpty.dll`/`OpenConsole.exe` pair next to the
application when available, then falls back to the system ConPTY.

## Links

- [API documentation](https://docs.rs/conpty-oxide)
- [Examples](https://github.com/P4suta/conpty-oxide/tree/main/examples)
- [ConPTY lifecycle notes](https://github.com/P4suta/conpty-oxide/blob/main/docs/conpty-pitfalls.md)
- [Changelog](https://github.com/P4suta/conpty-oxide/blob/main/CHANGELOG.md)
- [Contributing](https://github.com/P4suta/conpty-oxide/blob/main/CONTRIBUTING.md)
- [Security policy](https://github.com/P4suta/conpty-oxide/security/policy)

Licensed under either Apache-2.0 or MIT, at your option.
