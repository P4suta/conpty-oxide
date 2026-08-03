// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Verifies the four committed public API snapshots and the feature-invariance
//! rules, or deliberately accepts the current API as the pre-1.0 baseline.
//!
//! Every supported feature shape is rendered with `cargo public-api` under the
//! pinned nightly toolchain and compared byte for byte against the snapshots
//! in `public-api/`. Two invariants are enforced on top: the `tracing` feature
//! must never change a shape's API, and the default feature set must be
//! identical to the `blocking` shape.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};

use crate::util::repository_root;

/// The nightly toolchain also used by the docs.rs-equivalent documentation
/// build; `cargo public-api` needs a nightly rustdoc.
const TOOLCHAIN: &str = "nightly-2026-07-02";

/// Snapshot names and the crate features that produce them.
const SHAPES: [(&str, Option<&str>); 4] = [
    ("no-features", None),
    ("blocking", Some("blocking")),
    ("tokio", Some("tokio")),
    ("all-frontends", Some("blocking,tokio")),
];

/// Substrings that reveal a private dependency in rendered API text.
const DEPENDENCY_MARKERS: [&str; 5] = [
    "windows_spawn",
    "windows_sys",
    "thiserror",
    "mio::",
    "socket2::",
];

const UTF8_BOM: &str = "\u{feff}";

pub fn run(arguments: &[String]) -> Result<()> {
    let update = match arguments {
        [] => false,
        [flag] if flag == "--update" => true,
        _ => bail!("public-api accepts only the optional `--update` flag"),
    };

    let snapshot_directory = repository_root()?.join("public-api");
    if update && !snapshot_directory.exists() {
        fs::create_dir_all(&snapshot_directory)
            .with_context(|| format!("failed to create {}", snapshot_directory.display()))?;
    }

    let mut blocking_api = None;
    for (name, features) in SHAPES {
        let api = render_public_api(&shape_arguments(features, false))?;
        assert_no_dependency_leak(name, &api)?;
        if name == "blocking" {
            blocking_api = Some(api.clone());
        }

        let snapshot_path = snapshot_directory.join(format!("{name}.txt"));
        if update {
            write_snapshot(&snapshot_path, &api)?;
        } else {
            let snapshot = read_snapshot(&snapshot_path)?;
            ensure!(
                snapshot == api,
                "Public API changed for {name}. Review it, then run 'just public-api-update' \
                 to accept it."
            );
        }

        let tracing_api = render_public_api(&shape_arguments(features, true))?;
        ensure!(
            tracing_api == api,
            "The tracing feature changes the {name} public API"
        );
    }

    let default_api = render_public_api(&[])?;
    ensure!(
        Some(default_api) == blocking_api,
        "The default public API must be identical to the blocking feature shape"
    );

    if update {
        println!("Updated four public API snapshots.");
    } else {
        println!("Public API snapshots and feature invariants are current.");
    }
    Ok(())
}

fn shape_arguments(features: Option<&str>, tracing: bool) -> Vec<String> {
    let mut arguments = vec![String::from("--no-default-features")];
    let selected = match (features, tracing) {
        (Some(features), true) => Some(format!("{features},tracing")),
        (Some(features), false) => Some(String::from(features)),
        (None, true) => Some(String::from("tracing")),
        (None, false) => None,
    };
    if let Some(selected) = selected {
        arguments.push(String::from("--features"));
        arguments.push(selected);
    }
    arguments
}

/// Renders the public API of the root crate for one feature selection.
fn render_public_api(arguments: &[String]) -> Result<String> {
    let output = Command::new("cargo")
        .arg(format!("+{TOOLCHAIN}"))
        .args(["public-api", "--color", "never", "--omit", "blanket-impls"])
        .args(arguments)
        .current_dir(repository_root()?)
        .output()
        .context("failed to run cargo public-api")?;
    if !output.status.success() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        bail!(
            "cargo public-api failed with exit code {}",
            output.status.code().unwrap_or(1)
        );
    }
    let mut api = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    if !api.ends_with('\n') {
        api.push('\n');
    }
    Ok(api)
}

fn assert_no_dependency_leak(name: &str, api: &str) -> Result<()> {
    ensure!(
        !DEPENDENCY_MARKERS.iter().any(|marker| api.contains(marker)),
        "{name} exposes a private dependency type"
    );

    let dependency_api = api.replace("conpty_oxide::tokio::", "");
    let tokio_lines: Vec<&str> = dependency_api
        .lines()
        .filter(|line| line.contains("tokio::"))
        .collect();
    if name != "tokio" && name != "all-frontends" {
        ensure!(
            tokio_lines.is_empty(),
            "{name} unexpectedly exposes a Tokio type"
        );
    }
    for line in tokio_lines {
        ensure!(
            line.contains("tokio::io::"),
            "{name} exposes a Tokio type outside the intentional I/O trait contract: {line}"
        );
    }
    Ok(())
}

/// Reads a snapshot for comparison, tolerating the byte-order mark the
/// previous PowerShell updater wrote.
fn read_snapshot(path: &PathBuf) -> Result<String> {
    ensure!(
        path.exists(),
        "Missing public API snapshot: {}",
        path.display()
    );
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(text.strip_prefix(UTF8_BOM).unwrap_or(&text).to_owned())
}

/// Writes a snapshot with the same byte-order mark the previous PowerShell
/// updater produced, so an unchanged API round-trips to identical bytes.
fn write_snapshot(path: &PathBuf, api: &str) -> Result<()> {
    fs::write(path, format!("{UTF8_BOM}{api}"))
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{assert_no_dependency_leak, shape_arguments};

    #[test]
    fn shape_arguments_cover_features_and_tracing() {
        assert_eq!(shape_arguments(None, false), ["--no-default-features"]);
        assert_eq!(
            shape_arguments(None, true),
            ["--no-default-features", "--features", "tracing"]
        );
        assert_eq!(
            shape_arguments(Some("blocking,tokio"), true),
            [
                "--no-default-features",
                "--features",
                "blocking,tokio,tracing"
            ]
        );
    }

    #[test]
    fn dependency_markers_are_rejected() {
        let api = "pub fn leak() -> windows_sys::Foundation::HANDLE\n";
        let error = assert_no_dependency_leak("blocking", api).unwrap_err();
        assert!(error.to_string().contains("private dependency type"));
    }

    #[test]
    fn tokio_types_are_rejected_outside_tokio_shapes() {
        let api = "impl tokio::io::AsyncRead for conpty_oxide::tokio::Session\n";
        let error = assert_no_dependency_leak("blocking", api).unwrap_err();
        assert!(error
            .to_string()
            .contains("unexpectedly exposes a Tokio type"));
        assert!(assert_no_dependency_leak("tokio", api).is_ok());
    }

    #[test]
    fn tokio_types_outside_the_io_contract_are_rejected() {
        let api = "pub fn handle() -> tokio::runtime::Handle\n";
        let error = assert_no_dependency_leak("tokio", api).unwrap_err();
        assert!(error
            .to_string()
            .contains("outside the intentional I/O trait contract"));
    }

    #[test]
    fn the_session_trait_impl_form_is_accepted() {
        let api = "impl tokio::io::AsyncWrite for conpty_oxide::tokio::OwnedWriteHalf\n";
        assert!(assert_no_dependency_leak("all-frontends", api).is_ok());
    }
}
