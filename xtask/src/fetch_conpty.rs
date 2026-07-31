// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lays out a `conpty.dll` / `OpenConsole.exe` bundle for the external-backend
//! tests.
//!
//! Downloads the pinned `Microsoft.Windows.Console.ConPTY` NuGet package (MIT,
//! published by the microsoft/terminal team), verifies its SHA-256, and
//! extracts the two files the external backend needs into one directory.
//! `conpty.dll` launches `OpenConsole.exe` rather than the OS `conhost.exe`
//! and looks for it next to itself first, so a single directory is all the
//! deployment there is. The two must come from the same package: a mismatched
//! pair crashes the client process instead of degrading, which is why
//! `ConPtyBackend::from_dir` refuses one and why this subcommand verifies the
//! `ProductVersion` resources agree before it reports success.
//!
//! The package archive is kept so a re-run (and CI, via actions/cache) does
//! not download it again. The checksum is verified on every run, cached or
//! not.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

use crate::util::repository_root;

/// Pinned so that CI, developer machines, and the recorded checksum cannot
/// drift apart.
const DEFAULT_VERSION: &str = "1.24.260710001";

/// Expected SHA-256 of the `.nupkg`, as published by nuget.org.
const DEFAULT_SHA256: &str = "175640566A3B59C4B132070EE96C2C77E5AB7EDD2E92732A5EB3610BBF63D90E";

const PACKAGE_ID: &str = "Microsoft.Windows.Console.ConPTY";

#[derive(Debug)]
struct Arguments {
    version: String,
    sha256: String,
    arch: String,
    destination: PathBuf,
    package_directory: PathBuf,
}

pub fn run(arguments: &[String]) -> Result<()> {
    let arguments = parse_arguments(arguments)?;
    let package_path = arguments
        .package_directory
        .join(format!("{PACKAGE_ID}.{}.nupkg", arguments.version));

    // Entry names inside the package, confirmed against the real archive. The
    // console host lives under build/ rather than runtimes/ because MSBuild
    // copies it into an architecture subdirectory of the output folder, and it
    // uses the bare architecture name where the DLL uses the `win-` prefixed
    // RID.
    let dll_entry = format!("runtimes/win-{}/native/conpty.dll", arguments.arch);
    let host_entry = format!("build/native/runtimes/{}/OpenConsole.exe", arguments.arch);

    download_package(&arguments, &package_path)?;
    verify_checksum(&arguments, &package_path)?;

    let destination = &arguments.destination;
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    extract_entry(&package_path, &arguments, &dll_entry, "conpty.dll")?;
    extract_entry(&package_path, &arguments, &host_entry, "OpenConsole.exe")?;

    let dll_version = product_version(&destination.join("conpty.dll"))?;
    let host_version = product_version(&destination.join("OpenConsole.exe"))?;
    ensure!(
        dll_version == host_version,
        "the extracted pair disagrees: conpty.dll reports {dll_version} but OpenConsole.exe \
         reports {host_version}"
    );

    println!();
    println!(
        "{PACKAGE_ID} {} ({}), ProductVersion {dll_version}",
        arguments.version, arguments.arch
    );
    println!("Bundle ready at {}", destination.display());
    println!("Run the external-backend tests with:");
    println!(
        "  $env:CONPTY_OXIDE_TEST_DLL_DIR = '{}'",
        destination.display()
    );
    Ok(())
}

fn parse_arguments(arguments: &[String]) -> Result<Arguments> {
    let repository = repository_root()?;
    let mut version = String::from(DEFAULT_VERSION);
    let mut sha256 = String::from(DEFAULT_SHA256);
    let mut arch = None;
    let mut destination = None;
    let mut package_directory = None;
    let mut remaining = arguments.iter();
    while let Some(flag) = remaining.next() {
        let value = remaining
            .next()
            .with_context(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--version" => version = value.clone(),
            "--sha256" => sha256 = value.clone(),
            "--arch" => {
                ensure!(
                    ["x64", "arm64", "x86"].contains(&value.as_str()),
                    "--arch must be x64, arm64, or x86"
                );
                arch = Some(value.clone());
            },
            "--destination" => destination = Some(PathBuf::from(value)),
            "--package-directory" => package_directory = Some(PathBuf::from(value)),
            other => bail!("unknown fetch-conpty flag `{other}`"),
        }
    }
    let arch = match arch {
        Some(arch) => arch,
        None => machine_architecture()?,
    };
    Ok(Arguments {
        version,
        sha256,
        arch,
        destination: destination.unwrap_or_else(|| repository.join("vendor/conpty")),
        package_directory: package_directory.unwrap_or_else(|| repository.join("vendor/.package")),
    })
}

/// Defaults to this machine's architecture, which is what `cargo test` builds
/// for.
fn machine_architecture() -> Result<String> {
    let processor = std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_default();
    match processor.as_str() {
        "AMD64" => Ok(String::from("x64")),
        "ARM64" => Ok(String::from("arm64")),
        "x86" => Ok(String::from("x86")),
        other => bail!("unsupported processor architecture '{other}'; pass --arch explicitly"),
    }
}

fn download_package(arguments: &Arguments, package_path: &Path) -> Result<()> {
    fs::create_dir_all(&arguments.package_directory)
        .with_context(|| format!("failed to create {}", arguments.package_directory.display()))?;

    if package_path.exists() {
        println!("Using the cached package at {}", package_path.display());
        return Ok(());
    }

    let url = format!(
        "https://www.nuget.org/api/v2/package/{PACKAGE_ID}/{}",
        arguments.version
    );
    println!("Downloading {url}");
    let response = ureq::get(&url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .with_context(|| format!("failed to read the body of {url}"))?;

    // Rename only once the body is complete, so an interrupted download cannot
    // be picked up as a cache hit on the next run.
    let partial = package_path.with_extension("nupkg.partial");
    fs::write(&partial, &body).with_context(|| format!("failed to write {}", partial.display()))?;
    fs::rename(&partial, package_path)
        .with_context(|| format!("failed to move the download to {}", package_path.display()))?;
    Ok(())
}

/// Verifies the checksum on every run; a cached file that no longer matches is
/// deleted so the next run re-downloads instead of failing forever.
fn verify_checksum(arguments: &Arguments, package_path: &Path) -> Result<()> {
    let body = fs::read(package_path)
        .with_context(|| format!("failed to read {}", package_path.display()))?;
    let actual = format!("{:X}", Sha256::digest(&body));
    let expected = arguments.sha256.to_uppercase();
    if actual != expected {
        fs::remove_file(package_path)
            .with_context(|| format!("failed to remove {}", package_path.display()))?;
        bail!(
            "SHA-256 mismatch for {PACKAGE_ID} {}\n  expected {}\n  actual   {actual}\nThe \
             cached package has been removed.",
            arguments.version,
            arguments.sha256
        );
    }
    println!("Verified SHA-256 {actual}");
    Ok(())
}

fn extract_entry(
    package_path: &Path,
    arguments: &Arguments,
    entry_name: &str,
    file_name: &str,
) -> Result<()> {
    let package = fs::File::open(package_path)
        .with_context(|| format!("failed to open {}", package_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(package).context("failed to open the package as an archive")?;
    let mut entry = match archive.by_name(entry_name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => bail!(
            "'{entry_name}' is missing from {PACKAGE_ID} {}",
            arguments.version
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read '{entry_name}'"));
        },
    };
    let mut body = Vec::new();
    entry
        .read_to_end(&mut body)
        .with_context(|| format!("failed to extract '{entry_name}'"))?;
    let target = arguments.destination.join(file_name);
    fs::write(&target, &body).with_context(|| format!("failed to write {}", target.display()))?;
    println!("Extracted {entry_name} -> {}", target.display());
    Ok(())
}

/// Reads the `ProductVersion` string resource both binaries must agree on.
fn product_version(path: &Path) -> Result<String> {
    let map = pelite::FileMap::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let file = pelite::PeFile::from_bytes(&map)
        .with_context(|| format!("failed to parse {} as a PE image", path.display()))?;
    let resources = file
        .resources()
        .with_context(|| format!("{} carries no resource section", path.display()))?;
    let version_info = resources
        .version_info()
        .with_context(|| format!("{} carries no version resource", path.display()))?;
    let language = version_info
        .translation()
        .first()
        .with_context(|| format!("{} carries no version translation", path.display()))?;
    let product_version = version_info
        .value(*language, "ProductVersion")
        .with_context(|| format!("'{}' carries no ProductVersion resource", path.display()))?;
    Ok(product_version.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_arguments;

    #[test]
    fn defaults_follow_the_repository_layout() {
        let arguments = parse_arguments(&[]).unwrap();
        assert_eq!(arguments.version, super::DEFAULT_VERSION);
        assert!(arguments.destination.ends_with("vendor/conpty"));
        assert!(arguments.package_directory.ends_with("vendor/.package"));
    }

    #[test]
    fn unknown_architectures_are_rejected() {
        let flags = [String::from("--arch"), String::from("mips")];
        let error = parse_arguments(&flags).unwrap_err();
        assert!(error.to_string().contains("--arch must be"));
    }
}
