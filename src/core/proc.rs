// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one-way adapter from ConPTY lifecycle ownership to windows-spawn.

use std::io;

use windows_spawn::{DropPolicy, Job, SpawnOptions};

use crate::command::Command;
use crate::core::pseudocon::SpawnCapability;

/// Spawns through windows-spawn, the sole command-line, environment, Job,
/// attribute-list, and `CreateProcessW` implementation.
pub(super) fn spawn(
    command: &mut Command,
    pseudoconsole: &SpawnCapability<'_>,
    job: &Job,
) -> io::Result<windows_spawn::Child> {
    let options = SpawnOptions::new()
        .job(job)
        .pseudoconsole(pseudoconsole)
        .drop_policy(DropPolicy::Detach);
    command.windows_spawn_mut().spawn_with(options)
}
