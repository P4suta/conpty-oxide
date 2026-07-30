// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pipe creation facade for the blocking and Tokio frontends.
//!
//! A pseudoconsole session needs two unidirectional byte streams:
//!
//! - **conout** — the pseudoconsole writes rendered VT output into its end;
//!   the frontend reads ours.
//! - **conin** — the frontend writes user input into our end; the
//!   pseudoconsole reads its end.
//!
//! The concrete implementations are feature-local. [`anonymous`] creates
//! synchronous `CreatePipe` pairs for the blocking frontend, while
//! [`overlapped`] creates named-pipe pairs whose server ends can be registered
//! with Tokio's I/O driver and whose ConPTY-facing client ends remain
//! synchronous.

#[cfg(any(feature = "blocking", test))]
mod anonymous;
#[cfg(any(feature = "tokio", test))]
mod overlapped;

#[cfg(any(feature = "blocking", test))]
pub(crate) use anonymous::{create_sync_pipes, SyncPipes};
#[cfg(feature = "tokio")]
pub(crate) use overlapped::{create_overlapped_pipes, OverlappedPipes};
