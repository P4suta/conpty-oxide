//! Correctness-first Windows ConPTY (pseudoconsole) library.
//!
//! `conpty-oxide` wraps the Windows pseudoconsole (ConPTY) API with a focus on
//! getting the hard parts right:
//!
//! - A well-defined EOF contract for the console output pipe.
//! - No hangs around `ClosePseudoConsole`.
//! - Reliable process-tree termination ("kill tree") via Job objects.
//! - A blocking API (default `blocking` feature) and an async API behind the
//!   `tokio` feature.
//! - Dynamic loading of `conpty.dll`, falling back to the system console API.
//!
//! This crate targets Windows exclusively and does not compile on other
//! platforms.

#[cfg(not(windows))]
compile_error!(
    "conpty-oxide only supports Windows targets; \
     build it with a `*-pc-windows-*` target."
);

mod error;
mod size;

pub use error::{BackendError, Error, Result};
pub use size::Size;
