<!--
SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# conpty-oxide

A Windows ConPTY (pseudoconsole) library for Rust with blocking and Tokio APIs.

The crate manages pseudoconsole lifetime, process-tree termination, output
draining, and end-of-file behavior. It supports Windows 10 version 1809 and
later. The minimum supported Rust version is 1.75.

## Features

- Blocking and Tokio front ends with matching managed-session APIs.
- Process-tree cleanup through Windows Job objects.
- Consistent output and EOF handling across legacy and current ConPTY hosts.
- Optional loading of a validated `conpty.dll` and `OpenConsole.exe` pair.
- Optional lifecycle diagnostics through `tracing`.

## Installation

The default feature enables the blocking API:

```toml
[dependencies]
conpty-oxide = "0.1"
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

Available crate features are `blocking`, `tokio`, and `tracing`.

## Blocking

```rust
use conpty_oxide::blocking::Command;

fn main() -> conpty_oxide::Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .output()?;

    assert!(output.status().success());
    print!("{}", String::from_utf8_lossy(output.as_bytes()));
    Ok(())
}
```

## Tokio

```rust
use conpty_oxide::tokio::Command;

#[tokio::main]
async fn main() -> conpty_oxide::Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .output()
        .await?;

    assert!(output.status().success());
    print!("{}", String::from_utf8_lossy(output.as_bytes()));
    Ok(())
}
```

Use `Command::spawn()` for an interactive managed session. Drain output while
the child is running, and keep the input half alive until the child exits;
closing input requests pseudoconsole shutdown. `Command::output()` and
`Session::wait_with_output()` handle these rules for collected output.

## Backend selection

By default, the crate uses a valid `conpty.dll`/`OpenConsole.exe` pair next to
the executable when present, then falls back to the operating-system ConPTY.
Use `ConPtyBackend::from_dir` and `SessionOptions::backend` to select a bundle
explicitly. Both files must come from the same
[`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
package.

## Documentation

- [API documentation](https://docs.rs/conpty-oxide)
- [Examples](examples)
- [ConPTY lifecycle and compatibility notes](docs/conpty-pitfalls.md)
- [Release and artifact verification](docs/releasing.md)
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)

Version 0.1 is Windows-only and intentionally excludes detached sessions,
manual EOF policy, cursor inheritance, pre-staged spawning, and a
cross-platform facade. Breaking changes remain possible before 1.0.

## Development

```powershell
just setup
just ci
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the complete workflow.

## License

Licensed under either the [Apache License 2.0](LICENSE-APACHE) or the
[MIT License](LICENSE-MIT), at your option.
