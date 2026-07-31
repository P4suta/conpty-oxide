// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Contributor automation for the conpty-oxide repository.
//!
//! Each subcommand replaces a former PowerShell script from `scripts/`. The
//! crate is detached from the published package's workspace on purpose: it may
//! evolve freely without touching the crate's lockfile or release checks.

mod source_policy;

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let result = match arguments.first().map(String::as_str) {
        Some("source-policy") => source_policy::run(),
        Some(other) => Err(anyhow::anyhow!(
            "unknown xtask subcommand `{other}`; available: source-policy"
        )),
        None => Err(anyhow::anyhow!(
            "usage: cargo run --manifest-path xtask/Cargo.toml -- <subcommand>"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        },
    }
}
