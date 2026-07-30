// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runtime-independent low-level PTY builder configuration.

use crate::{ConPtyBackend, Size};

/// Settings shared by the blocking and Tokio `PtyBuilder` facades.
#[derive(Debug, Clone)]
pub(crate) struct PtyOptions {
    pub(crate) size: Size,
    pub(crate) backend: Option<ConPtyBackend>,
    pub(crate) inherit_cursor: bool,
    pub(crate) eof_on_root_exit: bool,
}

impl Default for PtyOptions {
    fn default() -> Self {
        Self {
            size: Size::default(),
            backend: None,
            inherit_cursor: false,
            eof_on_root_exit: true,
        }
    }
}
