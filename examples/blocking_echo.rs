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

use std::error::Error;
use std::io::{self, Read, Write};
use std::thread;

use conpty_oxide::blocking::{Command, Pty};
use conpty_oxide::Size;

fn main() -> Result<(), Box<dyn Error>> {
    let pty = Pty::builder().size(Size::new(24, 80)).build()?;

    let mut child = Command::new("cmd.exe")
        .args(["/c", "echo", "hello"])
        .spawn(&pty)?;

    // The pseudoconsole's output must be drained while the child runs, or a
    // full pipe buffer would wedge the session. The write half goes unused
    // here, but closing it early would make the console host tear the session
    // down and cut the child's output short, so it is kept until the end.
    let (mut reader, writer, _controller) = pty.into_split();
    let output = thread::spawn(move || {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map(|_| buf)
    });

    let status = child.wait()?;
    // Joining only returns once the session reached end-of-file, which is what
    // the crate guarantees on every supported Windows version.
    let output = output.join().expect("the reader thread must not panic")?;
    drop(writer);

    io::stdout().write_all(&output)?;
    io::stdout().flush()?;
    println!("\n{status}");

    Ok(())
}
