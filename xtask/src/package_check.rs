// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Inspects and smoke-tests the exact normalized package Cargo would publish.
//!
//! Creates the `.crate` archive with Cargo, extracts that archive with the
//! `tar` and `flate2` crates (no PATH-resolved `tar.exe`), verifies its
//! required and forbidden paths and every relative Markdown link, checks every
//! supported feature shape, then generates independent blocking and Tokio
//! consumers from the templates in `xtask/templates/` and runs them with the
//! minimum supported Rust version against the extracted source.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use regex::Regex;

use crate::util::repository_root;

const REQUIRED_PATHS: [&str; 17] = [
    "Cargo.lock",
    "Cargo.toml",
    "Cargo.toml.orig",
    "CHANGELOG.md",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "LICENSES/Apache-2.0.txt",
    "LICENSES/CC0-1.0.txt",
    "LICENSES/MIT.txt",
    "README.md",
    "REUSE.toml",
    "docs/conpty-pitfalls.md",
    "examples/blocking_echo.rs",
    "examples/tokio_interactive.rs",
    "src/lib.rs",
    "tests/managed_session.rs",
    "tests/public_api.rs",
];

const FORBIDDEN_PATHS: [&str; 16] = [
    ".cargo/",
    ".github/",
    ".gitignore",
    ".tools/",
    "docs/adr/",
    "docs/releasing.md",
    "justfile",
    "lefthook.yml",
    "mise.lock",
    "mise.toml",
    "mutants.out",
    "public-api/",
    "scripts/",
    "target/",
    "vendor/",
    "xtask/",
];

struct Consumer {
    directory: &'static str,
    manifest: &'static str,
    main: &'static str,
}

const CONSUMERS: [Consumer; 2] = [
    Consumer {
        directory: "blocking",
        manifest: include_str!("../templates/blocking-consumer.Cargo.toml.in"),
        main: include_str!("../templates/blocking-consumer-main.rs.in"),
    },
    Consumer {
        directory: "tokio",
        manifest: include_str!("../templates/tokio-consumer.Cargo.toml.in"),
        main: include_str!("../templates/tokio-consumer-main.rs.in"),
    },
];

pub fn run() -> Result<()> {
    let root = repository_root()?;
    let workspace = root.join("target/package-check");

    let (name, version, target_directory) = root_package(root)?;
    run_checked(
        "cargo",
        &["package", "--locked", "--allow-dirty", "--no-verify"],
        root,
        &[],
        "Creating the normalized Cargo package",
    )?;

    let archive = target_directory.join(format!("package/{name}-{version}.crate"));
    ensure!(
        archive.is_file(),
        "cargo package did not create {}",
        archive.display()
    );

    if workspace.exists() {
        fs::remove_dir_all(&workspace)
            .with_context(|| format!("failed to remove {}", workspace.display()))?;
    }
    let source_parent = workspace.join("source");
    fs::create_dir_all(&source_parent)
        .with_context(|| format!("failed to create {}", source_parent.display()))?;

    println!("Extracting the .crate archive");
    extract_crate(&archive, &source_parent)?;
    let package_source = source_parent.join(format!("{name}-{version}"));
    ensure!(
        package_source.is_dir(),
        "the archive did not contain the expected {} root",
        package_source.display()
    );

    let mut files = Vec::new();
    collect_relative_files(&package_source, &package_source, &mut files)?;
    files.sort();

    for path in REQUIRED_PATHS {
        ensure!(
            files.iter().any(|file| file == path),
            "the published package is missing required path '{path}'"
        );
    }
    for path in FORBIDDEN_PATHS {
        let prefix = path.trim_end_matches('/');
        let found = files
            .iter()
            .find(|file| file.as_str() == prefix || file.starts_with(path));
        if let Some(found) = found {
            bail!("the published package contains forbidden path '{found}'");
        }
    }
    println!("Package contents passed ({} files).", files.len());

    check_markdown_links(&package_source)?;
    println!("Published Markdown links passed.");

    let build_directory = workspace.join("build");
    let build_env = [(
        "CARGO_TARGET_DIR",
        build_directory.to_string_lossy().into_owned(),
    )];
    let manifest = package_source.join("Cargo.toml");
    let manifest = manifest.to_string_lossy();
    let shapes: [(&str, &[&str]); 5] = [
        ("default", &[]),
        ("no-features", &["--no-default-features"]),
        (
            "blocking",
            &["--no-default-features", "--features", "blocking"],
        ),
        ("tokio", &["--no-default-features", "--features", "tokio"]),
        ("all-features", &["--all-features"]),
    ];
    for (shape_name, shape_arguments) in shapes {
        let mut arguments = vec![
            "check",
            "--manifest-path",
            &manifest,
            "--locked",
            "--all-targets",
        ];
        arguments.extend_from_slice(shape_arguments);
        run_checked(
            "cargo",
            &arguments,
            &package_source,
            &build_env,
            &format!("Checking normalized package shape '{shape_name}'"),
        )?;
    }

    let dependency_path = package_source.to_string_lossy().replace('\\', "/");
    let lock_version_pattern = Regex::new(r"(?m)^version = (?P<version>[0-9]+)\r?$")
        .context("lockfile-version pattern")?;
    for consumer in CONSUMERS {
        let consumer_root = workspace.join("consumers").join(consumer.directory);
        fs::create_dir_all(consumer_root.join("src"))
            .with_context(|| format!("failed to create {}", consumer_root.display()))?;
        write_normalized(
            &consumer_root.join("Cargo.toml"),
            &consumer
                .manifest
                .replace("@DEPENDENCY_PATH@", &dependency_path),
        )?;
        write_normalized(&consumer_root.join("src/main.rs"), consumer.main)?;

        let consumer_manifest = consumer_root.join("Cargo.toml");
        let consumer_manifest = consumer_manifest.to_string_lossy();
        let consumer_lock = consumer_root.join("Cargo.lock");
        ensure!(
            !consumer_lock.exists(),
            "external consumer unexpectedly has a stale lockfile: {}",
            consumer_lock.display()
        );

        run_checked(
            "cargo",
            &[
                "+1.75.0",
                "generate-lockfile",
                "--manifest-path",
                &consumer_manifest,
            ],
            &consumer_root,
            &build_env,
            &format!(
                "Locking external consumer '{}' with Cargo 1.75",
                consumer_root.display()
            ),
        )?;

        let lock_contents = fs::read_to_string(&consumer_lock)
            .with_context(|| format!("failed to read {}", consumer_lock.display()))?;
        let lock_version = lock_version_pattern
            .captures(&lock_contents)
            .map(|found| found["version"].to_owned());
        ensure!(
            lock_version.as_deref() == Some("3"),
            "Cargo 1.75 did not create a version 3 lockfile at {}",
            consumer_lock.display()
        );

        run_checked(
            "cargo",
            &[
                "+1.75.0",
                "run",
                "--manifest-path",
                &consumer_manifest,
                "--locked",
                "--quiet",
            ],
            &consumer_root,
            &build_env,
            &format!(
                "Running external consumer '{}' with Rust 1.75",
                consumer_root.display()
            ),
        )?;
    }

    println!("Normalized package, feature shapes, and external consumers passed.");
    Ok(())
}

/// Reads the root package's name, version, and target directory from Cargo.
fn root_package(root: &Path) -> Result<(String, String, PathBuf)> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    ensure!(
        output.status.success(),
        "cargo metadata failed with exit code {}",
        output.status.code().unwrap_or(1)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata")?;

    let root_manifest = root.join("Cargo.toml");
    let packages: Vec<&serde_json::Value> = metadata["packages"]
        .as_array()
        .context("cargo metadata carries no package list")?
        .iter()
        .filter(|package| {
            package["manifest_path"]
                .as_str()
                .is_some_and(|path| Path::new(path) == root_manifest)
        })
        .collect();
    ensure!(
        packages.len() == 1,
        "expected one root package, found {}",
        packages.len()
    );
    let name = packages[0]["name"]
        .as_str()
        .context("the root package has no name")?;
    let version = packages[0]["version"]
        .as_str()
        .context("the root package has no version")?;
    let target_directory = metadata["target_directory"]
        .as_str()
        .context("cargo metadata carries no target directory")?;
    Ok((
        name.to_owned(),
        version.to_owned(),
        PathBuf::from(target_directory),
    ))
}

fn run_checked(
    program: &str,
    arguments: &[&str],
    working_directory: &Path,
    environment: &[(&str, String)],
    description: &str,
) -> Result<()> {
    println!("{description}");
    let mut command = Command::new(program);
    command.args(arguments).current_dir(working_directory);
    for (key, value) in environment {
        command.env(key, value);
    }
    let status = command
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    ensure!(
        status.success(),
        "{program} failed with exit code {} while: {description}",
        status.code().unwrap_or(1)
    );
    Ok(())
}

fn extract_crate(archive: &Path, destination: &Path) -> Result<()> {
    let file =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    tar::Archive::new(decoder)
        .unpack(destination)
        .with_context(|| format!("failed to extract {}", archive.display()))
}

fn collect_relative_files(root: &Path, directory: &Path, files: &mut Vec<String>) -> Result<()> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_relative_files(root, &path, files)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("path escaped the package root: {}", path.display()))?;
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Writes generated consumer sources with Unix newlines, matching the
/// template bytes regardless of checkout normalization.
fn write_normalized(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content.replace("\r\n", "\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Verifies that every relative Markdown link in the published package points
/// at a file that ships inside the package.
fn check_markdown_links(package_source: &Path) -> Result<()> {
    let link = Regex::new(r"\]\((?P<destination><[^>]+>|[^)\s]+)").context("link pattern")?;
    let absolute_uri = Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:").context("uri pattern")?;
    let mut markdown_files = Vec::new();
    collect_markdown_files(package_source, &mut markdown_files)?;

    for markdown_file in markdown_files {
        let contents = fs::read_to_string(&markdown_file)
            .with_context(|| format!("failed to read {}", markdown_file.display()))?;
        let directory = markdown_file
            .parent()
            .with_context(|| format!("{} has no parent", markdown_file.display()))?;
        for found in link.captures_iter(&contents) {
            let destination = found["destination"].trim_matches(['<', '>']);
            if destination.starts_with('#') || absolute_uri.is_match(destination) {
                continue;
            }
            let relative = destination.split(['?', '#']).next().unwrap_or_default();
            if relative.trim().is_empty() {
                continue;
            }
            let target = normalize_lexically(&directory.join(percent_decode(relative)));
            let contained = target.starts_with(package_source) && target.exists();
            if !contained {
                let source = markdown_file
                    .strip_prefix(package_source)
                    .unwrap_or(&markdown_file)
                    .to_string_lossy()
                    .replace('\\', "/");
                bail!("published Markdown '{source}' has missing relative link '{destination}'");
            }
        }
    }
    Ok(())
}

fn collect_markdown_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_markdown_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
    Ok(())
}

/// Resolves `.` and `..` components without touching the filesystem, so a
/// link that climbs out of the package is caught even when its target exists.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                normalized.pop();
            },
            other => normalized.push(other),
        }
    }
    normalized
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let base = bytes[index];
        let escape = (base == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit())
        .then(|| u8::from_str_radix(&text[index + 1..index + 3], 16).ok())
        .flatten();
        if let Some(value) = escape {
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(base);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{normalize_lexically, percent_decode};

    #[test]
    fn lexical_normalization_resolves_dot_components() {
        let normalized = normalize_lexically(Path::new("C:/pkg/docs/../README.md"));
        assert_eq!(normalized, Path::new("C:/pkg/README.md"));
    }

    #[test]
    fn parent_components_can_escape_the_package() {
        let normalized = normalize_lexically(Path::new("C:/pkg/../../outside.md"));
        assert!(!normalized.starts_with("C:/pkg"));
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(percent_decode("a%20b.md"), "a b.md");
        assert_eq!(percent_decode("plain.md"), "plain.md");
        assert_eq!(percent_decode("broken%2"), "broken%2");
        assert_eq!(percent_decode("multi%éx"), "multi%éx");
    }
}
