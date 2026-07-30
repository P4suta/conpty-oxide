# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
#
# SPDX-License-Identifier: MIT OR Apache-2.0

[CmdletBinding()]
param(
    [switch] $Update
)

$ErrorActionPreference = 'Stop'
$toolchain = 'nightly-2026-07-02'
$snapshotDirectory = Join-Path (Split-Path -Parent $PSScriptRoot) 'public-api'

$shapes = @(
    [PSCustomObject]@{ Name = 'no-features'; Features = $null },
    [PSCustomObject]@{ Name = 'blocking'; Features = 'blocking' },
    [PSCustomObject]@{ Name = 'tokio'; Features = 'tokio' },
    [PSCustomObject]@{ Name = 'all-frontends'; Features = 'blocking,tokio' }
)

function Invoke-PublicApi {
    param(
        [string[]] $Arguments
    )

    $cargoArguments = @(
        "+$toolchain",
        'public-api',
        '--color',
        'never',
        '--omit',
        'blanket-impls'
    )
    $cargoArguments += $Arguments
    $lines = @(& cargo @cargoArguments)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo public-api failed with exit code $LASTEXITCODE"
    }

    return ($lines -join "`n") + "`n"
}

function Get-ShapeArgument {
    param(
        [AllowNull()]
        [string] $Features,
        [switch] $Tracing
    )

    $arguments = @('--no-default-features')
    $selectedFeatures = $Features
    if ($Tracing) {
        if ($selectedFeatures) {
            $selectedFeatures = "$selectedFeatures,tracing"
        } else {
            $selectedFeatures = 'tracing'
        }
    }
    if ($selectedFeatures) {
        $arguments += @('--features', $selectedFeatures)
    }
    return $arguments
}

function Assert-NoDependencyLeak {
    param(
        [string] $Name,
        [string] $Api
    )

    if ($Api -match '(windows_sys|thiserror|mio::|socket2::)') {
        throw "$Name exposes a private dependency type"
    }

    $dependencyApi = $Api.Replace('conpty_oxide::tokio::', '')
    $tokioLines = @($dependencyApi -split "`n" | Where-Object { $_ -match 'tokio::' })
    if ($Name -notin @('tokio', 'all-frontends') -and $tokioLines.Count -ne 0) {
        throw "$Name unexpectedly exposes a Tokio type"
    }
    foreach ($line in $tokioLines) {
        if ($line -notmatch 'tokio::io::') {
            throw "$Name exposes a Tokio type outside the intentional I/O trait contract: $line"
        }
    }
}

if ($Update -and -not (Test-Path -LiteralPath $snapshotDirectory)) {
    New-Item -ItemType Directory -Path $snapshotDirectory | Out-Null
}

$generated = @{}
foreach ($shape in $shapes) {
    $arguments = Get-ShapeArgument -Features $shape.Features
    $api = Invoke-PublicApi -Arguments $arguments
    Assert-NoDependencyLeak -Name $shape.Name -Api $api
    $generated[$shape.Name] = $api

    $snapshotPath = Join-Path $snapshotDirectory "$($shape.Name).txt"
    if ($Update) {
        Set-Content -LiteralPath $snapshotPath -Value $api -NoNewline -Encoding utf8
    } elseif (-not (Test-Path -LiteralPath $snapshotPath)) {
        throw "Missing public API snapshot: $snapshotPath"
    } elseif ((Get-Content -LiteralPath $snapshotPath -Raw) -cne $api) {
        throw "Public API changed for $($shape.Name). Review it, then run 'just public-api-update' to accept it."
    }

    $tracingArguments = Get-ShapeArgument -Features $shape.Features -Tracing
    $tracingApi = Invoke-PublicApi -Arguments $tracingArguments
    if ($tracingApi -cne $api) {
        throw "The tracing feature changes the $($shape.Name) public API"
    }
}

$defaultApi = Invoke-PublicApi -Arguments @()
if ($defaultApi -cne $generated.blocking) {
    throw 'The default public API must be identical to the blocking feature shape'
}

if ($Update) {
    Write-Information 'Updated four public API snapshots.' -InformationAction Continue
} else {
    Write-Information 'Public API snapshots and feature invariants are current.' -InformationAction Continue
}
