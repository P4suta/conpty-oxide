// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Verifies a published, immutable conpty-oxide release.
//!
//! Downloads the five release assets and verifies the GitHub release
//! attestation, each release asset, SHA-256 checksums, the crates.io sparse
//! index checksum, CycloneDX metadata, and both online and bundled artifact
//! attestations. The only external command required is GitHub CLI (`gh`).
//!
//! Progress is reported on stderr; on success, the absolute download
//! directory is the only line written to stdout, so wrappers can capture the
//! verified location as the subcommand's single output value.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

const CRATE_NAME: &str = "conpty-oxide";
const PROVENANCE_PREDICATE_TYPE: &str = "https://slsa.dev/provenance/v1";
const CYCLONEDX_PREDICATE_TYPE: &str = "https://cyclonedx.org/bom";
const SPARSE_INDEX_URI: &str = "https://index.crates.io/co/np/conpty-oxide";

struct Arguments {
    tag: Option<String>,
    repository: String,
    output_directory: Option<PathBuf>,
}

pub fn run(arguments: &[String]) -> Result<()> {
    let arguments = parse_arguments(arguments)?;
    let repository = &arguments.repository;
    let signer_workflow = format!("{repository}/.github/workflows/release-finalize.yml");

    run_gh(
        &["auth", "status", "--hostname", "github.com"],
        "Checking GitHub CLI authentication",
    )?;

    let release = resolve_release(&arguments)?;
    let resolved_tag = required_str(&release, "tagName", "Release")?;
    if let Some(tag) = &arguments.tag {
        ensure!(
            &resolved_tag == tag,
            "Requested tag '{tag}' resolved as unexpected tag '{resolved_tag}'."
        );
    }
    let version = release_version(&resolved_tag)?;

    ensure!(
        !required_bool(&release, "isDraft", "Release")?,
        "Release '{resolved_tag}' is still a draft."
    );
    ensure!(
        required_bool(&release, "isImmutable", "Release")?,
        "Release '{resolved_tag}' is not immutable."
    );
    let published_at = required_str(&release, "publishedAt", "Release")?;
    ensure!(
        !published_at.trim().is_empty(),
        "Release '{resolved_tag}' has no publication timestamp."
    );

    let crate_asset = format!("{CRATE_NAME}-{version}.crate");
    let sbom_asset = format!("{CRATE_NAME}-{version}.cdx.json");
    let checksum_asset = String::from("SHA256SUMS");
    let provenance_bundle = format!("{CRATE_NAME}-{version}.provenance.sigstore.json");
    let sbom_bundle = format!("{CRATE_NAME}-{version}.sbom.sigstore.json");
    let expected_assets = [
        &crate_asset,
        &sbom_asset,
        &checksum_asset,
        &provenance_bundle,
        &sbom_bundle,
    ];

    let asset_names: Vec<String> = release["assets"]
        .as_array()
        .context("Release is missing required property 'assets'.")?
        .iter()
        .map(|asset| required_str(asset, "name", "Release asset"))
        .collect::<Result<_>>()?;
    ensure!(
        asset_names.len() == expected_assets.len(),
        "Release '{resolved_tag}' must contain exactly {} assets; found {}: {}",
        expected_assets.len(),
        asset_names.len(),
        asset_names.join(", ")
    );
    for expected in expected_assets {
        ensure!(
            asset_names.iter().any(|name| name == expected),
            "Release '{resolved_tag}' is missing expected asset '{expected}'."
        );
    }

    let download_directory = prepare_download_directory(&arguments, &version)?;
    let mut download_arguments = vec![
        "release",
        "download",
        &resolved_tag,
        "--repo",
        repository,
        "--dir",
    ];
    let directory_text = download_directory.to_string_lossy().into_owned();
    download_arguments.push(&directory_text);
    for expected in expected_assets {
        download_arguments.push("--pattern");
        download_arguments.push(expected);
    }
    run_gh(
        &download_arguments,
        &format!("Downloading the five assets for release '{resolved_tag}'"),
    )?;

    for expected in expected_assets {
        ensure!(
            download_directory.join(expected).is_file(),
            "GitHub CLI did not download expected asset '{expected}'."
        );
    }

    run_gh(
        &["release", "verify", &resolved_tag, "--repo", repository],
        &format!("Verifying immutable release attestation for '{resolved_tag}'"),
    )?;
    for expected in expected_assets {
        let asset_path = download_directory.join(expected);
        run_gh(
            &[
                "release",
                "verify-asset",
                &resolved_tag,
                &asset_path.to_string_lossy(),
                "--repo",
                repository,
            ],
            &format!("Verifying release attestation for asset '{expected}'"),
        )?;
    }

    let crate_sha256 = verify_checksums(
        &download_directory,
        &checksum_asset,
        &[&crate_asset, &sbom_asset, &provenance_bundle, &sbom_bundle],
        &crate_asset,
    )?;
    eprintln!("SHA256SUMS verified all four non-manifest assets.");

    verify_sparse_index(&version, &crate_sha256)?;
    eprintln!("The release .crate matches the crates.io sparse index checksum.");

    let sbom_path = download_directory.join(&sbom_asset);
    let sbom = verify_cyclonedx_document(&sbom_path, &sbom_asset, &version, &crate_sha256)?;
    eprintln!("CycloneDX root component and crates.io distribution metadata passed.");

    let tag_commit = resolve_tag_commit(repository, &resolved_tag)?;
    let crate_path = download_directory.join(&crate_asset);
    verify_vcs_info(&crate_path, &version, &tag_commit)?;
    eprintln!("The release .crate records the tagged commit {tag_commit}.");

    let provenance_bundle_path = download_directory.join(&provenance_bundle);
    let sbom_bundle_path = download_directory.join(&sbom_bundle);

    let attestation = AttestationContext {
        repository,
        signer_workflow: &signer_workflow,
    };
    let attest = |artifact: &Path, predicate: &str, bundle: Option<&Path>, description: &str| {
        verify_attestation(&attestation, artifact, predicate, bundle, description)
    };
    attest(
        &crate_path,
        PROVENANCE_PREDICATE_TYPE,
        None,
        "Verifying online SLSA provenance for the .crate asset",
    )?;
    attest(
        &crate_path,
        PROVENANCE_PREDICATE_TYPE,
        Some(&provenance_bundle_path),
        "Verifying bundled SLSA provenance for the .crate asset",
    )?;
    attest(
        &sbom_path,
        PROVENANCE_PREDICATE_TYPE,
        None,
        "Verifying online SLSA provenance for the CycloneDX asset",
    )?;
    attest(
        &sbom_path,
        PROVENANCE_PREDICATE_TYPE,
        Some(&provenance_bundle_path),
        "Verifying bundled SLSA provenance for the CycloneDX asset",
    )?;

    let online_results = attest(
        &crate_path,
        CYCLONEDX_PREDICATE_TYPE,
        None,
        "Verifying the online CycloneDX attestation for the .crate asset",
    )?;
    assert_sbom_predicates_equal(&online_results, &sbom, "Online CycloneDX attestation")?;
    let bundled_results = attest(
        &crate_path,
        CYCLONEDX_PREDICATE_TYPE,
        Some(&sbom_bundle_path),
        "Verifying the bundled CycloneDX attestation for the .crate asset",
    )?;
    assert_sbom_predicates_equal(&bundled_results, &sbom, "Bundled CycloneDX attestation")?;

    eprintln!("Release '{resolved_tag}' passed all integrity and provenance checks.");
    println!("{}", download_directory.display());
    Ok(())
}

fn parse_arguments(arguments: &[String]) -> Result<Arguments> {
    let repository_pattern =
        Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").context("repository pattern")?;
    let mut tag = None;
    let mut repository = String::from("P4suta/conpty-oxide");
    let mut output_directory = None;
    let mut remaining = arguments.iter();
    while let Some(flag) = remaining.next() {
        let value = remaining
            .next()
            .with_context(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            // An empty value keeps the "latest published release" default so
            // wrappers can always pass the flag.
            "--tag" => tag = (!value.trim().is_empty()).then(|| value.clone()),
            "--repository" => {
                ensure!(
                    repository_pattern.is_match(value),
                    "--repository must use the OWNER/REPOSITORY form"
                );
                repository = value.clone();
            },
            "--output-directory" => {
                output_directory = (!value.trim().is_empty()).then(|| PathBuf::from(value));
            },
            other => bail!("unknown verify-release flag `{other}`"),
        }
    }
    Ok(Arguments {
        tag,
        repository,
        output_directory,
    })
}

/// Runs GitHub CLI, echoing the description to stderr; returns stdout.
fn run_gh(arguments: &[&str], description: &str) -> Result<String> {
    eprintln!("{description}");
    let output = Command::new("gh")
        .args(arguments)
        .output()
        .context("GitHub CLI (gh) was not found on PATH.")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let mut details = stdout.clone();
        details.push_str(&String::from_utf8_lossy(&output.stderr));
        let details = if details.trim().is_empty() {
            "GitHub CLI produced no diagnostic output."
        } else {
            details.trim()
        };
        bail!(
            "GitHub CLI failed with exit code {} while: {description}\n{details}",
            output.status.code().unwrap_or(1)
        );
    }
    Ok(stdout)
}

fn run_gh_json(arguments: &[&str], description: &str) -> Result<Value> {
    let json = run_gh(arguments, description)?;
    ensure!(
        !json.trim().is_empty(),
        "GitHub CLI returned no JSON while: {description}"
    );
    serde_json::from_str(&json)
        .with_context(|| format!("GitHub CLI returned invalid JSON while: {description}"))
}

fn required_str(value: &Value, name: &str, context: &str) -> Result<String> {
    value[name]
        .as_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("{context} is missing required property '{name}'."))
}

fn required_bool(value: &Value, name: &str, context: &str) -> Result<bool> {
    value[name]
        .as_bool()
        .with_context(|| format!("{context} is missing required property '{name}'."))
}

fn resolve_release(arguments: &Arguments) -> Result<Value> {
    let json_fields = "tagName,isDraft,isImmutable,isPrerelease,publishedAt,assets,url";
    let mut view_arguments = vec!["release", "view"];
    if let Some(tag) = &arguments.tag {
        view_arguments.push(tag);
    }
    view_arguments.extend_from_slice(&["--repo", &arguments.repository, "--json", json_fields]);
    let description = match &arguments.tag {
        Some(tag) => format!("Resolving GitHub release '{tag}'"),
        None => String::from("Resolving the latest published GitHub release"),
    };
    let release = run_gh_json(&view_arguments, &description);
    match (&arguments.tag, release) {
        (None, Err(error)) => Err(error).with_context(|| {
            format!(
                "No latest published release could be resolved for {}.",
                arguments.repository
            )
        }),
        (_, release) => release,
    }
}

fn release_version(resolved_tag: &str) -> Result<String> {
    let tag_pattern = Regex::new(
        r"^v(?P<version>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)$",
    )
    .context("tag pattern")?;
    let found = tag_pattern.captures(resolved_tag).with_context(|| {
        format!("Release tag '{resolved_tag}' is not a supported v-prefixed semantic version.")
    })?;
    Ok(found["version"].to_owned())
}

fn prepare_download_directory(arguments: &Arguments, version: &str) -> Result<PathBuf> {
    let directory = match &arguments.output_directory {
        Some(directory) => directory.clone(),
        None => std::env::temp_dir().join(format!(
            "conpty-oxide-release-{version}-{}",
            std::process::id()
        )),
    };
    ensure!(
        !directory.is_file(),
        "Output path '{}' is a file, not a directory.",
        directory.display()
    );
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let entries = fs::read_dir(&directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
        .count();
    ensure!(
        entries == 0,
        "Output directory '{}' must be empty.",
        directory.display()
    );
    directory
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", directory.display()))
}

/// Parses and enforces SHA256SUMS, returning the `.crate` asset's SHA-256.
fn verify_checksums(
    download_directory: &Path,
    checksum_asset: &str,
    targets: &[&String],
    crate_asset: &str,
) -> Result<String> {
    let line_pattern = Regex::new(r"^(?P<hash>[0-9A-Fa-f]{64}) (?P<mode>[ *])(?P<name>.+)$")
        .context("checksum pattern")?;
    let contents = fs::read_to_string(download_directory.join(checksum_asset))
        .context("failed to read SHA256SUMS")?;
    let mut checksums: Vec<(String, String)> = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let found = line_pattern
            .captures(line)
            .with_context(|| format!("SHA256SUMS contains malformed line: '{line}'"))?;
        let name = found["name"].to_owned();
        let file_name_only = !name.contains('/')
            && !name.contains('\\')
            && Path::new(&name)
                .file_name()
                .is_some_and(|file_name| file_name.to_string_lossy() == name.as_str());
        ensure!(
            file_name_only,
            "SHA256SUMS contains unsafe asset name '{name}'."
        );
        ensure!(
            checksums.iter().all(|(existing, _)| existing != &name),
            "SHA256SUMS lists asset '{name}' more than once."
        );
        checksums.push((name, found["hash"].to_lowercase()));
    }
    ensure!(
        checksums.len() == targets.len(),
        "SHA256SUMS must contain exactly {} entries; found {}.",
        targets.len(),
        checksums.len()
    );
    for target in targets {
        let expected = checksums
            .iter()
            .find(|(name, _)| name == *target)
            .map(|(_, hash)| hash.clone())
            .with_context(|| format!("SHA256SUMS is missing '{target}'."))?;
        let body = fs::read(download_directory.join(target.as_str()))
            .with_context(|| format!("failed to read {target}"))?;
        let actual = format!("{:x}", Sha256::digest(&body));
        ensure!(
            actual == expected,
            "SHA-256 mismatch for '{target}': expected {expected}, got {actual}."
        );
    }
    for (name, _) in &checksums {
        ensure!(
            targets.contains(&name),
            "SHA256SUMS contains unexpected entry '{name}'."
        );
    }
    checksums
        .iter()
        .find(|(name, _)| name == crate_asset)
        .map(|(_, hash)| hash.clone())
        .context("SHA256SUMS is missing the .crate entry")
}

fn verify_sparse_index(version: &str, crate_sha256: &str) -> Result<()> {
    eprintln!("Checking crates.io sparse index entry for {CRATE_NAME} {version}");
    let response = ureq::get(SPARSE_INDEX_URI)
        .set("Accept", "text/plain")
        .set("User-Agent", "conpty-oxide-release-verifier/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .with_context(|| {
            format!("Failed to download crates.io sparse index entry '{SPARSE_INDEX_URI}'.")
        })?;
    let body = response
        .into_string()
        .context("failed to read the crates.io sparse index response")?;

    let mut matching = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line)
            .context("crates.io sparse index returned malformed JSON.")?;
        if entry["vers"].as_str() == Some(version) {
            matching.push(entry);
        }
    }
    ensure!(
        matching.len() == 1,
        "Expected one crates.io sparse index entry for {CRATE_NAME} {version}; found {}.",
        matching.len()
    );
    let index_checksum = required_str(&matching[0], "cksum", "crates.io sparse index entry")?;
    ensure!(
        index_checksum.to_lowercase() == crate_sha256,
        "GitHub release .crate SHA-256 '{crate_sha256}' differs from crates.io index checksum \
         '{index_checksum}'."
    );
    if matching[0]["yanked"].as_bool() == Some(true) {
        eprintln!(
            "WARNING: {CRATE_NAME} {version} is yanked on crates.io; integrity checks will \
             continue."
        );
    }
    Ok(())
}

/// Validates the CycloneDX document's format, root component, and
/// distribution metadata against the released `.crate` digest.
fn verify_cyclonedx_document(
    sbom_path: &Path,
    sbom_asset: &str,
    version: &str,
    crate_sha256: &str,
) -> Result<Value> {
    let contents = fs::read_to_string(sbom_path)
        .with_context(|| format!("failed to read {}", sbom_path.display()))?;
    let sbom: Value = serde_json::from_str(&contents)
        .with_context(|| format!("CycloneDX asset '{sbom_asset}' is not valid JSON."))?;

    let bom_format = required_str(&sbom, "bomFormat", "CycloneDX document")?;
    ensure!(
        bom_format == "CycloneDX",
        "SBOM has unexpected bomFormat '{bom_format}'."
    );
    let spec_version = required_str(&sbom, "specVersion", "CycloneDX document")?;
    ensure!(
        spec_version == "1.5",
        "SBOM has CycloneDX specVersion '{spec_version}', expected '1.5'."
    );

    let root = &sbom["metadata"]["component"];
    ensure!(
        root.is_object(),
        "CycloneDX metadata is missing required property 'component'."
    );
    let root_name = required_str(root, "name", "CycloneDX root component")?;
    let root_version = required_str(root, "version", "CycloneDX root component")?;
    ensure!(
        root_name == CRATE_NAME,
        "CycloneDX root component name is '{root_name}', expected '{CRATE_NAME}'."
    );
    ensure!(
        root_version == version,
        "CycloneDX root component version is '{root_version}', expected '{version}'."
    );

    let root_hash = single_sha256(&root["hashes"], "CycloneDX root component")?;
    ensure!(
        root_hash.to_lowercase() == crate_sha256,
        "CycloneDX root SHA-256 '{root_hash}' differs from .crate SHA-256 '{crate_sha256}'."
    );

    let references = root["externalReferences"]
        .as_array()
        .context("CycloneDX root component is missing required property 'externalReferences'.")?;
    let distributions: Vec<&Value> = references
        .iter()
        .filter(|reference| reference["type"].as_str() == Some("distribution"))
        .collect();
    ensure!(
        distributions.len() == 1,
        "CycloneDX root component must have exactly one distribution reference; found {}.",
        distributions.len()
    );
    let distribution_url =
        required_str(distributions[0], "url", "CycloneDX distribution reference")?;
    let expected_url = format!("https://crates.io/api/v1/crates/{CRATE_NAME}/{version}/download");
    ensure!(
        distribution_url == expected_url,
        "CycloneDX distribution URL is '{distribution_url}', expected '{expected_url}'."
    );
    let distribution_hash = single_sha256(
        &distributions[0]["hashes"],
        "CycloneDX distribution reference",
    )?;
    ensure!(
        distribution_hash.to_lowercase() == crate_sha256,
        "CycloneDX distribution SHA-256 '{distribution_hash}' differs from .crate SHA-256 \
         '{crate_sha256}'."
    );
    Ok(sbom)
}

fn single_sha256(hashes: &Value, context: &str) -> Result<String> {
    let hashes = hashes
        .as_array()
        .with_context(|| format!("{context} is missing required property 'hashes'."))?;
    let sha256: Vec<&Value> = hashes
        .iter()
        .filter(|hash| hash["alg"].as_str() == Some("SHA-256"))
        .collect();
    ensure!(
        sha256.len() == 1,
        "{context} must have exactly one SHA-256 hash; found {}.",
        sha256.len()
    );
    required_str(sha256[0], "content", context)
}

/// Resolves a release tag to the lowercase commit ID it points at,
/// dereferencing up to eight nested annotated tags.
fn resolve_tag_commit(repository: &str, resolved_tag: &str) -> Result<String> {
    let object_id = Regex::new(r"^[0-9a-fA-F]{40,64}$").context("object-id pattern")?;
    let escaped: String = resolved_tag
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                String::from(byte as char)
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let reference = run_gh_json(
        &["api", &format!("repos/{repository}/git/ref/tags/{escaped}")],
        &format!("Resolving Git tag '{resolved_tag}'"),
    )?;
    let mut object_type = required_str(&reference["object"], "type", "Git tag reference object")?;
    let mut object_sha = required_str(&reference["object"], "sha", "Git tag reference object")?;

    let mut depth = 0;
    while object_type == "tag" {
        ensure!(
            depth < 8,
            "Tag '{resolved_tag}' contains more than eight nested annotated tags."
        );
        ensure!(
            object_id.is_match(&object_sha),
            "Tag '{resolved_tag}' contains invalid Git object ID '{object_sha}'."
        );
        let tag_object = run_gh_json(
            &["api", &format!("repos/{repository}/git/tags/{object_sha}")],
            &format!("Dereferencing annotated tag object {object_sha}"),
        )?;
        object_type = required_str(&tag_object["object"], "type", "Annotated tag target")?;
        object_sha = required_str(&tag_object["object"], "sha", "Annotated tag target")?;
        depth += 1;
    }

    ensure!(
        object_type == "commit",
        "Tag '{resolved_tag}' resolves to a Git {object_type} object, not a commit."
    );
    ensure!(
        object_id.is_match(&object_sha),
        "Tag '{resolved_tag}' resolves to invalid commit ID '{object_sha}'."
    );

    let commit = run_gh_json(
        &["api", &format!("repos/{repository}/commits/{object_sha}")],
        &format!("Confirming tag commit {object_sha}"),
    )?;
    let confirmed = required_str(&commit, "sha", "Commit")?;
    ensure!(
        confirmed == object_sha,
        "GitHub resolved commit '{object_sha}' as unexpected commit '{confirmed}'."
    );
    Ok(object_sha.to_lowercase())
}

/// Requires the crate archive's `.cargo_vcs_info.json` to record exactly the
/// tagged release commit, packaged from a clean working tree.
fn verify_vcs_info(crate_path: &Path, version: &str, tag_commit: &str) -> Result<()> {
    let entry_name = format!("{CRATE_NAME}-{version}/.cargo_vcs_info.json");
    let file = fs::File::open(crate_path)
        .with_context(|| format!("failed to open {}", crate_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .context("failed to enumerate the crate archive")?;
    for entry in entries {
        let mut entry = entry.context("failed to read a crate archive entry")?;
        let path = entry.path().context("a crate archive entry has no path")?;
        if path.to_string_lossy().replace('\\', "/") != entry_name {
            continue;
        }
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut entry, &mut contents)
            .context("failed to read .cargo_vcs_info.json from the crate")?;
        let info: Value =
            serde_json::from_str(&contents).context(".cargo_vcs_info.json is not valid JSON")?;
        let recorded = required_str(&info["git"], "sha1", ".cargo_vcs_info.json git object")?;
        ensure!(
            recorded.eq_ignore_ascii_case(tag_commit),
            "The crate records commit {recorded}, but the tag resolves to {tag_commit}."
        );
        ensure!(
            !info["git"]["dirty"].as_bool().unwrap_or(false),
            "The crate was packaged from a dirty working tree."
        );
        return Ok(());
    }
    bail!("The crate archive contains no '{entry_name}'.");
}

/// The release-wide identity every attestation must verify against.
///
/// The finalizer's dispatch ref is deliberately not part of this identity:
/// the finalizer may run from `main` to repair an existing tag, and
/// `verify_vcs_info` already binds the crate bytes to the tagged commit.
struct AttestationContext<'a> {
    repository: &'a str,
    signer_workflow: &'a str,
}

fn verify_attestation(
    context: &AttestationContext<'_>,
    artifact: &Path,
    predicate_type: &str,
    bundle: Option<&Path>,
    description: &str,
) -> Result<Vec<Value>> {
    let artifact_text = artifact.to_string_lossy();
    let mut arguments = vec![
        "attestation",
        "verify",
        &artifact_text,
        "--repo",
        context.repository,
        "--predicate-type",
        predicate_type,
        "--signer-workflow",
        context.signer_workflow,
        "--deny-self-hosted-runners",
        "--limit",
        "100",
        "--format",
        "json",
    ];
    let bundle_text = bundle.map(|bundle| bundle.to_string_lossy().into_owned());
    if let Some(bundle_text) = &bundle_text {
        arguments.push("--bundle");
        arguments.push(bundle_text);
    }
    let results = run_gh_json(&arguments, description)?;
    let results = results
        .as_array()
        .cloned()
        .with_context(|| format!("GitHub CLI returned non-array JSON while: {description}"))?;
    ensure!(
        !results.is_empty(),
        "No matching attestation was verified while: {description}"
    );
    Ok(results)
}

/// Requires every verification result to carry the downloaded CycloneDX
/// document, byte-identical under structural comparison.
fn assert_sbom_predicates_equal(results: &[Value], expected: &Value, context: &str) -> Result<()> {
    ensure!(
        !results.is_empty(),
        "{context} returned no verification results."
    );
    for result in results {
        let statement = &result["verificationResult"]["statement"];
        let predicate_type = required_str(statement, "predicateType", context)?;
        ensure!(
            predicate_type == CYCLONEDX_PREDICATE_TYPE,
            "{context} returned unexpected predicate type '{predicate_type}'."
        );
        let predicate = &statement["predicate"];
        ensure!(
            !predicate.is_null(),
            "{context} is missing required property 'predicate'."
        );
        ensure!(
            predicate == expected,
            "{context} contains an SBOM predicate that differs from the downloaded CycloneDX \
             document."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{assert_sbom_predicates_equal, release_version, single_sha256};

    #[test]
    fn v_prefixed_semantic_versions_are_accepted() {
        assert_eq!(release_version("v0.1.0").unwrap(), "0.1.0");
        assert_eq!(release_version("v1.2.3-rc.1").unwrap(), "1.2.3-rc.1");
        assert!(release_version("0.1.0").is_err());
        assert!(release_version("v01.0.0").is_err());
    }

    #[test]
    fn exactly_one_sha256_hash_is_required() {
        let hashes = json!([{ "alg": "SHA-256", "content": "abc" }]);
        assert_eq!(single_sha256(&hashes, "test").unwrap(), "abc");
        let doubled = json!([
            { "alg": "SHA-256", "content": "abc" },
            { "alg": "SHA-256", "content": "def" }
        ]);
        assert!(single_sha256(&doubled, "test").is_err());
    }

    #[test]
    fn sbom_predicates_must_match_structurally() {
        let sbom = json!({ "bomFormat": "CycloneDX", "components": [1, 2] });
        let matching = json!([{
            "verificationResult": { "statement": {
                "predicateType": "https://cyclonedx.org/bom",
                "predicate": { "components": [1, 2], "bomFormat": "CycloneDX" }
            }}
        }]);
        assert!(assert_sbom_predicates_equal(matching.as_array().unwrap(), &sbom, "test").is_ok());

        let differing = json!([{
            "verificationResult": { "statement": {
                "predicateType": "https://cyclonedx.org/bom",
                "predicate": { "bomFormat": "CycloneDX", "components": [1] }
            }}
        }]);
        assert!(
            assert_sbom_predicates_equal(differing.as_array().unwrap(), &sbom, "test").is_err()
        );
    }
}
