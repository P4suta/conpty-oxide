# Conpty-oxide

Run Windows ConPTY sessions through blocking and Tokio Rust APIs.

## Installation

```sh
cargo add conpty-oxide
```

## Usage

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

## Documentation

[Read the documentation](docs/README.md).

## License

Licensed under either license:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [Massachusetts Institute of Technology (MIT) license](LICENSE-MIT)

Contributions use both licenses unless you state other terms.
