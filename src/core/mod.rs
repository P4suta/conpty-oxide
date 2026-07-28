//! Internal building blocks shared by the blocking and async front ends.
//!
//! Nothing in this module tree is part of the public API. It holds the
//! Windows-specific plumbing that both front ends need: the synchronous
//! anonymous pipes that carry the pseudoconsole's I/O streams, the
//! pseudoconsole lifecycle state machine built on top of them, the job object
//! that owns the child's process tree, the `CreateProcessW` call that attaches
//! a child to both, and child-process exit detection.
//!
//! # Why synchronous pipes
//!
//! `CreatePseudoConsole` documents `hInput` and `hOutput` as "restricted to
//! synchronous I/O", i.e. handles that do not require an `OVERLAPPED`
//! structure. Anonymous pipes from `CreatePipe` are always synchronous, so
//! they satisfy that requirement by construction. The `tokio` front end
//! therefore cannot hand tokio's overlapped named-pipe handles to ConPTY; it
//! services these synchronous handles from blocking worker threads instead.

// TODO: drop the allows once the public session layer consumes these modules
// from non-test code.
#[allow(dead_code)]
pub(crate) mod job;
pub(crate) mod pipes;
#[allow(dead_code)]
pub(crate) mod proc;
#[allow(dead_code)]
pub(crate) mod pseudocon;
#[allow(dead_code)]
pub(crate) mod wait;
