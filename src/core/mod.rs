//! Internal building blocks shared by the blocking and async front ends.
//!
//! Nothing in this module tree is part of the public API. It holds the
//! Windows-specific plumbing that both front ends need: the synchronous
//! anonymous pipes that carry the pseudoconsole's I/O streams, and (in later
//! phases) the pseudoconsole lifecycle and child-process handling built on
//! top of them.
//!
//! # Why synchronous pipes
//!
//! `CreatePseudoConsole` documents `hInput` and `hOutput` as "restricted to
//! synchronous I/O", i.e. handles that do not require an `OVERLAPPED`
//! structure. Anonymous pipes from `CreatePipe` are always synchronous, so
//! they satisfy that requirement by construction. The `tokio` front end
//! therefore cannot hand tokio's overlapped named-pipe handles to ConPTY; it
//! services these synchronous handles from blocking worker threads instead.

pub(crate) mod pipes;
