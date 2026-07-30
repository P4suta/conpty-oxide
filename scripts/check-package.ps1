# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
# SPDX-License-Identifier: MIT OR Apache-2.0

<#
.SYNOPSIS
    Inspects and smoke-tests the exact normalized package Cargo would publish.

.DESCRIPTION
    Creates the .crate archive with Cargo, extracts that archive, verifies its
    required and forbidden paths, checks every supported feature shape, then
    runs independent blocking and Tokio consumers against the extracted source.
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$workspace = Join-Path $repositoryRoot 'target/package-check'

function Invoke-Checked {
    param(
        [Parameter(Mandatory)]
        [string] $Program,

        [Parameter(Mandatory)]
        [string[]] $Arguments,

        [Parameter(Mandatory)]
        [string] $WorkingDirectory,

        [Parameter(Mandatory)]
        [string] $Description
    )

    Write-Information $Description -InformationAction Continue
    Push-Location $WorkingDirectory
    try {
        & $Program @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$Program failed with exit code $LASTEXITCODE while: $Description"
        }
    }
    finally {
        Pop-Location
    }
}

function Write-Utf8File {
    param(
        [Parameter(Mandatory)]
        [string] $Path,

        [Parameter(Mandatory)]
        [string] $Content
    )

    $encoding = [Text.UTF8Encoding]::new($false)
    [IO.File]::WriteAllText($Path, $Content.Replace("`r`n", "`n"), $encoding)
}

Push-Location $repositoryRoot
try {
    $metadataText = @(& cargo metadata --no-deps --format-version 1)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata failed with exit code $LASTEXITCODE"
    }
    $metadata = ConvertFrom-Json ($metadataText -join "`n")
    $rootManifest = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'Cargo.toml'))
    $packages = @($metadata.packages | Where-Object {
        [IO.Path]::GetFullPath($_.manifest_path) -eq $rootManifest
    })
    if ($packages.Count -ne 1) {
        throw "expected one root package, found $($packages.Count)"
    }
    $package = $packages[0]

    Invoke-Checked -Program 'cargo' `
        -Arguments @('package', '--locked', '--allow-dirty', '--no-verify') `
        -WorkingDirectory $repositoryRoot `
        -Description 'Creating the normalized Cargo package'

    $archive = Join-Path $metadata.target_directory "package/$($package.name)-$($package.version).crate"
    if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
        throw "cargo package did not create $archive"
    }

    if (Test-Path -LiteralPath $workspace) {
        Remove-Item -LiteralPath $workspace -Recurse -Force
    }
    $sourceParent = Join-Path $workspace 'source'
    New-Item -ItemType Directory -Path $sourceParent -Force | Out-Null
    Invoke-Checked -Program 'tar.exe' `
        -Arguments @('-xzf', $archive, '-C', $sourceParent) `
        -WorkingDirectory $repositoryRoot `
        -Description 'Extracting the .crate archive'

    $packageSource = Join-Path $sourceParent "$($package.name)-$($package.version)"
    if (-not (Test-Path -LiteralPath $packageSource -PathType Container)) {
        throw "the archive did not contain the expected $packageSource root"
    }

    $sourcePrefix = [IO.Path]::GetFullPath($packageSource).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $files = @(Get-ChildItem -LiteralPath $packageSource -Recurse -Force -File | ForEach-Object {
        $_.FullName.Substring($sourcePrefix.Length).Replace('\', '/')
    })

    $required = @(
        'Cargo.lock',
        'Cargo.toml',
        'Cargo.toml.orig',
        'CHANGELOG.md',
        'LICENSE-APACHE',
        'LICENSE-MIT',
        'LICENSES/Apache-2.0.txt',
        'LICENSES/CC0-1.0.txt',
        'LICENSES/MIT.txt',
        'README.md',
        'REUSE.toml',
        'docs/conpty-pitfalls.md',
        'docs/mutation-testing.md',
        'docs/releasing.md',
        'examples/blocking_echo.rs',
        'examples/tokio_interactive.rs',
        'src/lib.rs',
        'tests/managed_session.rs',
        'tests/public_api.rs'
    )
    foreach ($path in $required) {
        if ($files -cnotcontains $path) {
            throw "the published package is missing required path '$path'"
        }
    }

    $forbidden = @(
        '.cargo/',
        '.github/',
        '.gitignore',
        '.tools/',
        'justfile',
        'lefthook.yml',
        'mise.lock',
        'mise.toml',
        'mutants.out',
        'public-api/',
        'scripts/',
        'target/',
        'vendor/'
    )
    foreach ($path in $forbidden) {
        $prefix = $path.TrimEnd('/')
        $forbiddenFiles = @($files | Where-Object {
            $_ -ceq $prefix -or $_.StartsWith($path, [StringComparison]::Ordinal)
        })
        if ($forbiddenFiles.Count -ne 0) {
            throw "the published package contains forbidden path '$($forbiddenFiles[0])'"
        }
    }
    Write-Information "Package contents passed ($($files.Count) files)." -InformationAction Continue

    $hadTargetDirectory = Test-Path Env:CARGO_TARGET_DIR
    $previousTargetDirectory = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = Join-Path $workspace 'build'
    try {
        $manifest = Join-Path $packageSource 'Cargo.toml'
        $shapes = @(
            [PSCustomObject]@{ Name = 'default'; Arguments = @() },
            [PSCustomObject]@{ Name = 'no-features'; Arguments = @('--no-default-features') },
            [PSCustomObject]@{
                Name = 'blocking'
                Arguments = @('--no-default-features', '--features', 'blocking')
            },
            [PSCustomObject]@{
                Name = 'tokio'
                Arguments = @('--no-default-features', '--features', 'tokio')
            },
            [PSCustomObject]@{ Name = 'all-features'; Arguments = @('--all-features') }
        )
        foreach ($shape in $shapes) {
            $arguments = @('check', '--manifest-path', $manifest, '--locked', '--all-targets')
            $arguments += $shape.Arguments
            Invoke-Checked -Program 'cargo' `
                -Arguments $arguments `
                -WorkingDirectory $packageSource `
                -Description "Checking normalized package shape '$($shape.Name)'"
        }

        $consumerRoot = Join-Path $workspace 'consumers'
        $blockingRoot = Join-Path $consumerRoot 'blocking'
        $tokioRoot = Join-Path $consumerRoot 'tokio'
        New-Item -ItemType Directory -Path (Join-Path $blockingRoot 'src') -Force | Out-Null
        New-Item -ItemType Directory -Path (Join-Path $tokioRoot 'src') -Force | Out-Null
        $dependencyPath = $packageSource.Replace('\', '/')

        Write-Utf8File -Path (Join-Path $blockingRoot 'Cargo.toml') -Content @"
[package]
name = "conpty-oxide-package-blocking-consumer"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies.conpty-oxide]
path = "$dependencyPath"
default-features = false
features = ["blocking"]
"@
        Write-Utf8File -Path (Join-Path $blockingRoot 'src/main.rs') -Content @'
use conpty_oxide::blocking::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "package-blocking-vt"])
        .spawn()?
        .wait_with_output()?;
    if output.status().code() != 0 {
        return Err(format!("unexpected exit code {}", output.status().code()).into());
    }
    let rendered = String::from_utf8_lossy(output.as_bytes());
    if !rendered.contains("package-blocking-vt") {
        return Err(format!("marker missing from VT output: {rendered:?}").into());
    }
    println!("blocking consumer passed");
    Ok(())
}
'@

        Write-Utf8File -Path (Join-Path $tokioRoot 'Cargo.toml') -Content @"
[package]
name = "conpty-oxide-package-tokio-consumer"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies.conpty-oxide]
path = "$dependencyPath"
default-features = false
features = ["tokio"]

[dependencies.tokio]
version = "1"
features = ["macros", "net", "rt"]
"@
        Write-Utf8File -Path (Join-Path $tokioRoot 'src/main.rs') -Content @'
use conpty_oxide::tokio::Command;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("cmd.exe")
        .args(["/c", "echo", "package-tokio-vt"])
        .spawn()?
        .wait_with_output()
        .await?;
    if output.status().code() != 0 {
        return Err(format!("unexpected exit code {}", output.status().code()).into());
    }
    let rendered = String::from_utf8_lossy(output.as_bytes());
    if !rendered.contains("package-tokio-vt") {
        return Err(format!("marker missing from VT output: {rendered:?}").into());
    }
    println!("Tokio consumer passed");
    Ok(())
}
'@

        foreach ($consumer in @($blockingRoot, $tokioRoot)) {
            $consumerManifest = Join-Path $consumer 'Cargo.toml'
            Invoke-Checked -Program 'cargo' `
                -Arguments @('generate-lockfile', '--manifest-path', $consumerManifest) `
                -WorkingDirectory $consumer `
                -Description "Locking external consumer '$consumer'"
            Invoke-Checked -Program 'cargo' `
                -Arguments @('run', '--manifest-path', $consumerManifest, '--locked', '--quiet') `
                -WorkingDirectory $consumer `
                -Description "Running external consumer '$consumer'"
        }
    }
    finally {
        if ($hadTargetDirectory) {
            $env:CARGO_TARGET_DIR = $previousTargetDirectory
        }
        else {
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
        }
    }
}
finally {
    Pop-Location
}

Write-Information 'Normalized package, feature shapes, and external consumers passed.' -InformationAction Continue
