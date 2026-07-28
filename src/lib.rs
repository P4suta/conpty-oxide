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
//!
//! # Where to start
//!
// The two paragraphs below are feature-gated so their intra-doc links always
// point at something that exists: neither front end is guaranteed to be
// compiled in, and a link into a module that was configured out is a rustdoc
// error rather than a dead link.
#![cfg_attr(
    feature = "blocking",
    doc = "[`blocking`] holds the synchronous API: build a [`blocking::Pty`], spawn a",
    doc = "[`blocking::Command`] into it, and read the session's output from a",
    doc = "dedicated thread. Its module documentation covers the two rules a",
    doc = "pseudoconsole imposes on every caller — service the output pipe from",
    doc = "another thread, and let the library decide when to close the console.",
    doc = ""
)]
#![cfg_attr(
    feature = "tokio",
    doc = "The `tokio` feature puts the same API at the crate root in asynchronous",
    doc = "form: a [`Pty`] implements `AsyncRead`/`AsyncWrite`, [`Child::wait`] is a",
    doc = "future, and the same rules apply to tasks instead of threads. The two",
    doc = "front ends are independent — their types do not mix — and either can be",
    doc = "compiled out. The [`asyn`] module documentation states the async",
    doc = "lifecycle rules in full.",
    doc = ""
)]
#![cfg_attr(
    not(feature = "tokio"),
    doc = "The `tokio` feature — not enabled in this build of the documentation —",
    doc = "puts the same API at the crate root in asynchronous form: a `Pty` that",
    doc = "implements `AsyncRead`/`AsyncWrite`, a `Child` whose `wait` is a future,",
    doc = "and an `asyn` module stating the async lifecycle rules in full.",
    doc = ""
)]
//! # Choosing a ConPTY implementation
//!
//! Sessions run on the operating system's own ConPTY unless told otherwise,
//! which needs no setup. An application that ships the standalone
//! `conpty.dll` (the `Microsoft.Windows.Console.ConPTY` NuGet package) can get
//! the newer console host's behaviour on older Windows versions instead:
//!
//! - [`ConPtyBackend::auto`] uses a bundle found next to the executable and
//!   falls back to the system implementation, which is what most applications
//!   want and what the default backend does.
//! - [`ConPtyBackend::from_dir`] loads a bundle from a directory you name,
//!   validating that its `conpty.dll` and `OpenConsole.exe` are a matching
//!   pair before either runs.
//! - [`ConPtyBackend::set_global_default`] installs the chosen backend for the
//!   whole process; a front end's `PtyBuilder::backend` sets it per session.

// Every internal layer of this crate — the command builder, the pipes, the
// pseudoconsole lifecycle, the spawn path — exists to serve a front end. With
// none compiled in (`--no-default-features` leaves only the public type
// definitions reachable) those layers legitimately have no consumer, so the
// dead-code lint is silenced in exactly that configuration. It stays active in
// every configuration that has a front end — blocking, async, or both — where
// an unused item really is a mistake.
#![cfg_attr(not(any(feature = "blocking", feature = "tokio")), allow(dead_code))]
// docs.rs passes `--cfg docsrs` (see `[package.metadata.docs.rs]`), which
// turns on the nightly-only `doc_auto_cfg` feature: every feature-gated item
// then carries an "Available on crate feature … only" badge. Stable builds
// never see the cfg, so this is inert everywhere else.
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
// Every public item carries documentation, and this keeps it that way. `warn`
// rather than `deny` so a half-written local edit still builds; every CI clippy
// invocation passes `-D warnings`, which promotes this to a hard error there.
// It lives here rather than in a `[lints.rust]` table so that it applies to the
// crate under any driver — `cargo rustc`, rustdoc, rust-analyzer — instead of
// only where Cargo forwards its lint configuration.
#![warn(missing_docs)]

#[cfg(not(windows))]
compile_error!(
    "conpty-oxide only supports Windows targets; \
     build it with a `*-pc-windows-*` target."
);

mod backend;
mod command;
mod core;
mod error;
mod size;
mod status;

#[cfg(feature = "blocking")]
pub mod blocking;

// Spelled `asyn` because `async` is a keyword. The module is public for the
// sake of its documentation — the async lifecycle rules and the async example
// live there, and the type docs refer to them — while its types are also
// re-exported at the root, so that `conpty_oxide::Pty` is the async session
// and `conpty_oxide::blocking::Pty` the synchronous one.
#[cfg(feature = "tokio")]
pub mod asyn;

#[cfg(feature = "tokio")]
pub use asyn::{
    Child, Command, OwnedReadHalf, OwnedWriteHalf, Pty, PtyBuilder, PtyController, ReadHalf,
    WriteHalf,
};

pub use backend::{BackendKind, ConPtyBackend};
pub use error::{BackendError, Error, Result};
pub use size::Size;
pub use status::ExitStatus;
