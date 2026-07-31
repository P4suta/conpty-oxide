// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Helpers shared by every xtask subcommand.

use std::path::Path;

use anyhow::{Context, Result};

/// Returns the repository root, which contains this tool's manifest directory.
pub fn repository_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("the xtask manifest directory has no parent")
}
