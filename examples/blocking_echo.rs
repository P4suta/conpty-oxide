// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal blocking example: run `cmd.exe /c echo hello` inside a
//! pseudoconsole and print what the console rendered.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example blocking_echo
//! ```
//!
//! The output is a virtual-terminal stream, so it carries the escape sequences
//! the console host emitted alongside the text; writing it straight to stdout
//! lets the terminal interpret them.

use std::io::{self, Write};

use conpty_oxide::blocking::Command;
use conpty_oxide::Result;

fn main() -> Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .spawn()?
        .collect_output()?;

    io::stdout().write_all(output.as_bytes())?;
    io::stdout().flush()?;
    println!("\n{}", output.status());

    Ok(())
}
