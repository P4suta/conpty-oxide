# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
# SPDX-License-Identifier: MIT OR Apache-2.0

<#
.SYNOPSIS
    Lays out a conpty.dll / OpenConsole.exe bundle for the external-backend tests.

.DESCRIPTION
    Downloads the pinned Microsoft.Windows.Console.ConPTY NuGet package
    (MIT, published by the microsoft/terminal team), verifies its SHA-256,
    and extracts the two files the external backend needs into a single
    directory:

        <Destination>/conpty.dll
        <Destination>/OpenConsole.exe

    conpty.dll launches OpenConsole.exe rather than the OS conhost.exe, and
    looks for it next to itself first, so putting both files in one directory
    is all the deployment there is. The two must come from the same package:
    a mismatched pair crashes the client process instead of degrading, which
    is why ConPtyBackend::from_dir refuses one and why this script verifies
    the ProductVersion resources agree before it reports success.

    The package archive is kept so a re-run (and CI, via actions/cache) does
    not download it again. The checksum is verified on every run, cached or
    not.

.PARAMETER Version
    Package version to fetch. Pinned so that CI, developer machines and the
    recorded checksum cannot drift apart.

.PARAMETER Sha256
    Expected SHA-256 of the .nupkg, as published by nuget.org. A mismatch
    aborts before anything is extracted.

.PARAMETER Arch
    Which architecture's binaries to lay out. Defaults to this machine's,
    which is what `cargo test` builds for.

.PARAMETER Destination
    Directory to write the bundle to. Defaults to <repo>/vendor/conpty, the
    path the test suite's CONPTY_OXIDE_TEST_DLL_DIR is meant to point at.

.PARAMETER PackageDirectory
    Where to keep the downloaded .nupkg. Defaults to <repo>/vendor/.package.

.EXAMPLE
    just fetch-conpty
    $env:CONPTY_OXIDE_TEST_DLL_DIR = "$PWD/vendor/conpty"
    cargo test --all-features
#>
[CmdletBinding()]
param(
    [string] $Version = '1.24.260710001',
    [string] $Sha256 = '175640566A3B59C4B132070EE96C2C77E5AB7EDD2E92732A5EB3610BBF63D90E',
    [ValidateSet('x64', 'arm64', 'x86')]
    [string] $Arch,
    [string] $Destination,
    [string] $PackageDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Without this a download over a slow link spends most of its time repainting
# a progress bar, and in CI it produces megabytes of useless log noise.
$ProgressPreference = 'SilentlyContinue'

# A launcher started from PowerShell 7 - `just`, whose Windows shell is
# `powershell.exe` - hands PowerShell 7's PSModulePath down to Windows
# PowerShell. Its autoloader then finds PowerShell 7's Core-only copies of the
# built-in modules, refuses them, and stops looking, so cmdlets as ordinary as
# Get-FileHash simply do not exist. Putting this host's own module directory
# back in front costs one line and makes the script work however it was
# launched.
if ($PSVersionTable.PSEdition -eq 'Desktop') {
    $env:PSModulePath = "$PSHOME\Modules;$env:PSModulePath"
}

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $Arch) {
    $Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'x64' }
        'ARM64' { 'arm64' }
        'x86' { 'x86' }
        default { throw "unsupported processor architecture '$($env:PROCESSOR_ARCHITECTURE)'; pass -Arch explicitly" }
    }
}
if (-not $Destination) { $Destination = Join-Path $repoRoot 'vendor/conpty' }
if (-not $PackageDirectory) { $PackageDirectory = Join-Path $repoRoot 'vendor/.package' }

$packageId = 'Microsoft.Windows.Console.ConPTY'
$packagePath = Join-Path $PackageDirectory "$packageId.$Version.nupkg"

# Entry names inside the package, confirmed against the real archive. The
# console host lives under build/ rather than runtimes/ because MSBuild copies
# it into an architecture subdirectory of the output folder, and it uses the
# bare architecture name where the DLL uses the `win-` prefixed RID.
$dllEntry = "runtimes/win-$Arch/native/conpty.dll"
$hostEntry = "build/native/runtimes/$Arch/OpenConsole.exe"

function Get-ProductVersion([string] $Path) {
    $info = (Get-Item -LiteralPath $Path).VersionInfo.ProductVersion
    if (-not $info) { throw "'$Path' carries no ProductVersion resource" }
    return $info.Trim()
}

# --- 1. Download (or reuse) the package -------------------------------------

New-Item -ItemType Directory -Force -Path $PackageDirectory | Out-Null

if (Test-Path -LiteralPath $packagePath) {
    Write-Information "Using the cached package at $packagePath" -InformationAction Continue
}
else {
    $url = "https://www.nuget.org/api/v2/package/$packageId/$Version"
    Write-Information "Downloading $url" -InformationAction Continue
    # Windows PowerShell 5.1 defaults to TLS 1.0, which nuget.org rejects.
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $partial = "$packagePath.partial"
    Invoke-WebRequest -Uri $url -OutFile $partial -UseBasicParsing
    # Rename only once the body is complete, so an interrupted download cannot
    # be picked up as a cache hit on the next run.
    Move-Item -LiteralPath $partial -Destination $packagePath -Force
}

# --- 2. Verify the checksum, every run --------------------------------------

$actual = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash
if ($actual -ne $Sha256.ToUpperInvariant()) {
    # A cached file that no longer matches is the interesting case: delete it
    # so the next run re-downloads instead of failing forever.
    Remove-Item -LiteralPath $packagePath -Force
    throw "SHA-256 mismatch for $packageId $Version`n  expected $Sha256`n  actual   $actual`nThe cached package has been removed."
}
Write-Information "Verified SHA-256 $actual" -InformationAction Continue

# --- 3. Extract the two files -----------------------------------------------

Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($packagePath)
try {
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($pair in @(@($dllEntry, 'conpty.dll'), @($hostEntry, 'OpenConsole.exe'))) {
        $entry = $archive.GetEntry($pair[0])
        if (-not $entry) { throw "'$($pair[0])' is missing from $packageId $Version" }
        $target = Join-Path $Destination $pair[1]
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
        Write-Information "Extracted $($pair[0]) -> $target" -InformationAction Continue
    }
}
finally {
    $archive.Dispose()
}

# --- 4. Prove the pair is consistent ----------------------------------------

$dllPath = Join-Path $Destination 'conpty.dll'
$hostPath = Join-Path $Destination 'OpenConsole.exe'
$dllVersion = Get-ProductVersion $dllPath
$hostVersion = Get-ProductVersion $hostPath
if ($dllVersion -ne $hostVersion) {
    throw "the extracted pair disagrees: conpty.dll reports $dllVersion but OpenConsole.exe reports $hostVersion"
}

Write-Information "" -InformationAction Continue
Write-Information "$packageId $Version ($Arch), ProductVersion $dllVersion" -InformationAction Continue
Write-Information "Bundle ready at $Destination" -InformationAction Continue
Write-Information "Run the external-backend tests with:" -InformationAction Continue
Write-Information "  `$env:CONPTY_OXIDE_TEST_DLL_DIR = '$Destination'" -InformationAction Continue
