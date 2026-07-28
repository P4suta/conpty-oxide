//! Interactive async example: run a shell inside a pseudoconsole and relay
//! this terminal's input and output to it.
//!
//! Run it with:
//!
//! ```text
//! cargo run --features tokio --example tokio_interactive
//! cargo run --features tokio --example tokio_interactive -- cmd.exe
//! ```
//!
//! Type into the shell as usual and end the session with `exit`. Because the
//! child's output is a virtual-terminal stream, writing it straight to stdout
//! lets this terminal interpret the escape sequences the console host emitted.
//!
//! Three things in here are the whole point of the example:
//!
//! - The session is split, so output is drained by one task while another
//!   feeds input — the pseudoconsole deadlocks if output is left unread.
//! - The write half is kept alive until the child has exited. Closing the
//!   input pipe is how a caller *ends* a session, not how it says "no more
//!   input"; doing it early kills the shell.
//! - Standard input is read on a plain thread rather than a Tokio blocking
//!   task, because a console read cannot be cancelled: the thread is simply
//!   abandoned at exit, whereas a blocking task would hold up the runtime's
//!   shutdown until somebody pressed a key.

use std::error::Error;
use std::io::{self, Read, Write};
use std::thread;

use conpty_oxide::{Command, Pty, Size};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Program launched when none is given on the command line.
const DEFAULT_SHELL: &str = "powershell.exe";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let shell = std::env::args().nth(1).unwrap_or_else(|| {
        // `powershell.exe` ships with every supported Windows version; the
        // argument is there for `cmd.exe`, `pwsh.exe`, or anything else.
        DEFAULT_SHELL.to_string()
    });

    // Must happen inside the runtime: the session's pipes are registered with
    // its I/O driver.
    let pty = Pty::builder().size(Size::new(24, 80)).build()?;
    let mut child = Command::new(&shell).spawn(&pty)?;
    let (mut reader, mut writer, _controller) = pty.into_split();

    // Console output -> our stdout. This task ends at end-of-file, which the
    // crate produces once the session is over on every supported Windows
    // version.
    let output = tokio::spawn(async move {
        let mut buf = vec![0u8; 8 * 1024];
        let mut stdout = io::stdout();
        loop {
            let read = reader.read(&mut buf).await?;
            if read == 0 {
                return io::Result::Ok(());
            }
            stdout.write_all(&buf[..read])?;
            stdout.flush()?;
        }
    });

    // Our stdin -> console input, over a channel fed by a detached thread.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    thread::Builder::new()
        .name("stdin-relay".into())
        .spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = vec![0u8; 4 * 1024];
            while let Ok(read) = stdin.read(&mut buf) {
                if read == 0 || input_tx.blocking_send(buf[..read].to_vec()).is_err() {
                    return;
                }
            }
        })?;

    // Owns the write half for the whole session, which is what keeps the
    // console host from treating a closed input pipe as a closed terminal.
    let input = tokio::spawn(async move {
        while let Some(chunk) = input_rx.recv().await {
            // A failed write means the console host is already gone, which the
            // exit status below reports properly; there is nothing to do here.
            if writer.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // Standard input is finished — redirected from a file, say — but the
        // write half deliberately stays alive. Dropping it here would tell the
        // console host that the terminal went away and kill the shell before
        // it had run the input it already received.
        std::future::pending::<()>().await
    });

    let status = child.wait().await?;
    // Returns at end-of-file, which the crate guarantees once the session is
    // over on every supported Windows version.
    output.await??;

    // The stdin relay is parked in a console read that cannot be cancelled, so
    // the input task can never finish on its own. Aborting it drops the write
    // half — the deliberate end of the session, and harmless now that the
    // child has already exited.
    input.abort();
    let _ = input.await;

    println!("{shell} exited: {status}");
    Ok(())
}
