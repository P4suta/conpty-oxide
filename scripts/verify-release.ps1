# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
# SPDX-License-Identifier: MIT OR Apache-2.0

<#
.SYNOPSIS
    Verifies a published, immutable conpty-oxide release.

.DESCRIPTION
    Downloads the five release assets and verifies the GitHub release
    attestation, each release asset, SHA-256 checksums, the crates.io sparse
    index checksum, CycloneDX metadata, and both online and bundled artifact
    attestations.

    The only external command required is GitHub CLI (`gh`). On success, the
    script writes the absolute download directory to the success stream.

.PARAMETER Tag
    Release tag to verify. If omitted, the latest published release is used.

.PARAMETER Repository
    GitHub repository in OWNER/REPOSITORY form.

.PARAMETER OutputDirectory
    Empty directory in which to download the verified assets. If omitted, a
    unique directory under the system temporary directory is created.

.EXAMPLE
    ./scripts/verify-release.ps1 -Tag v0.1.0

.EXAMPLE
    $directory = ./scripts/verify-release.ps1 -OutputDirectory ./verified-release
#>
[CmdletBinding()]
param(
    [Parameter()]
    [AllowEmptyString()]
    [string] $Tag,

    [Parameter()]
    [ValidatePattern('^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')]
    [string] $Repository = 'P4suta/conpty-oxide',

    [Parameter()]
    [AllowEmptyString()]
    [string] $OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$crateName = 'conpty-oxide'
$provenancePredicateType = 'https://slsa.dev/provenance/v1'
$cycloneDxPredicateType = 'https://cyclonedx.org/bom'
$signerWorkflow = "$Repository/.github/workflows/release-finalize.yml"

function Invoke-GhCommand {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,

        [Parameter(Mandatory)]
        [string] $Description
    )

    Write-Information $Description -InformationAction Continue
    $nativeOutput = @(& gh @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    $outputText = ($nativeOutput | ForEach-Object { $_.ToString() }) -join "`n"
    if ($exitCode -ne 0) {
        $details = if ([string]::IsNullOrWhiteSpace($outputText)) {
            'GitHub CLI produced no diagnostic output.'
        }
        else {
            $outputText
        }
        throw "GitHub CLI failed with exit code $exitCode while: $Description`n$details"
    }

    return $outputText
}

function Invoke-GhJson {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,

        [Parameter(Mandatory)]
        [string] $Description
    )

    $json = Invoke-GhCommand -Arguments $Arguments -Description $Description
    if ([string]::IsNullOrWhiteSpace($json)) {
        throw "GitHub CLI returned no JSON while: $Description"
    }

    try {
        return ($json | ConvertFrom-Json -AsHashtable)
    }
    catch {
        throw "GitHub CLI returned invalid JSON while: $Description`n$($_.Exception.Message)"
    }
}

function Get-RequiredDictionary {
    param(
        [Parameter(Mandatory)]
        [object] $Value,

        [Parameter(Mandatory)]
        [string] $Context
    )

    if ($Value -isnot [Collections.IDictionary]) {
        throw "$Context must be a JSON object."
    }

    return $Value
}

function Get-RequiredValue {
    param(
        [Parameter(Mandatory)]
        [Collections.IDictionary] $Object,

        [Parameter(Mandatory)]
        [string] $Name,

        [Parameter(Mandatory)]
        [string] $Context
    )

    if (-not $Object.Contains($Name) -or $null -eq $Object[$Name]) {
        throw "$Context is missing required property '$Name'."
    }

    return $Object[$Name]
}

function ConvertTo-CanonicalJson {
    param(
        [Parameter()]
        [AllowNull()]
        [object] $Value
    )

    if ($null -eq $Value) {
        return 'null'
    }

    if ($Value -is [Collections.IDictionary]) {
        [string[]] $keys = @($Value.Keys | ForEach-Object { [string] $_ })
        [Array]::Sort($keys, [StringComparer]::Ordinal)
        $members = foreach ($key in $keys) {
            $encodedKey = ConvertTo-Json -InputObject $key -Compress
            $encodedValue = ConvertTo-CanonicalJson -Value $Value[$key]
            "$encodedKey`:$encodedValue"
        }
        return '{' + ($members -join ',') + '}'
    }

    if ($Value -is [Collections.IEnumerable] -and $Value -isnot [string]) {
        $items = foreach ($item in $Value) {
            ConvertTo-CanonicalJson -Value $item
        }
        return '[' + ($items -join ',') + ']'
    }

    return (ConvertTo-Json -InputObject $Value -Compress)
}

function Resolve-TagCommit {
    param(
        [Parameter(Mandatory)]
        [string] $ResolvedTag
    )

    $escapedTag = [Uri]::EscapeDataString($ResolvedTag)
    $reference = Invoke-GhJson `
        -Arguments @('api', "repos/$Repository/git/ref/tags/$escapedTag") `
        -Description "Resolving Git tag '$ResolvedTag'"
    $referenceObject = Get-RequiredDictionary `
        -Value (Get-RequiredValue -Object $reference -Name 'object' -Context 'Git tag reference') `
        -Context 'Git tag reference object'
    $objectType = [string] (Get-RequiredValue `
        -Object $referenceObject -Name 'type' -Context 'Git tag reference object')
    $objectSha = [string] (Get-RequiredValue `
        -Object $referenceObject -Name 'sha' -Context 'Git tag reference object')

    for ($depth = 0; $objectType -eq 'tag'; $depth++) {
        if ($depth -ge 8) {
            throw "Tag '$ResolvedTag' contains more than eight nested annotated tags."
        }
        if ($objectSha -notmatch '^[0-9a-fA-F]{40,64}$') {
            throw "Tag '$ResolvedTag' contains invalid Git object ID '$objectSha'."
        }

        $tagObject = Invoke-GhJson `
            -Arguments @('api', "repos/$Repository/git/tags/$objectSha") `
            -Description "Dereferencing annotated tag object $objectSha"
        $target = Get-RequiredDictionary `
            -Value (Get-RequiredValue -Object $tagObject -Name 'object' -Context 'Annotated tag') `
            -Context 'Annotated tag target'
        $objectType = [string] (Get-RequiredValue `
            -Object $target -Name 'type' -Context 'Annotated tag target')
        $objectSha = [string] (Get-RequiredValue `
            -Object $target -Name 'sha' -Context 'Annotated tag target')
    }

    if ($objectType -ne 'commit') {
        throw "Tag '$ResolvedTag' resolves to a Git $objectType object, not a commit."
    }
    if ($objectSha -notmatch '^[0-9a-fA-F]{40,64}$') {
        throw "Tag '$ResolvedTag' resolves to invalid commit ID '$objectSha'."
    }

    $commit = Invoke-GhJson `
        -Arguments @('api', "repos/$Repository/commits/$objectSha") `
        -Description "Confirming tag commit $objectSha"
    $confirmedSha = [string] (Get-RequiredValue -Object $commit -Name 'sha' -Context 'Commit')
    if ($confirmedSha -cne $objectSha) {
        throw "GitHub resolved commit '$objectSha' as unexpected commit '$confirmedSha'."
    }

    return $objectSha.ToLowerInvariant()
}

function Invoke-AttestationVerification {
    param(
        [Parameter(Mandatory)]
        [string] $ArtifactPath,

        [Parameter(Mandatory)]
        [string] $PredicateType,

        [Parameter(Mandatory)]
        [string] $ResolvedTag,

        [Parameter(Mandatory)]
        [string] $SourceDigest,

        [Parameter()]
        [string] $BundlePath,

        [Parameter(Mandatory)]
        [string] $Description
    )

    $arguments = @(
        'attestation', 'verify', $ArtifactPath,
        '--repo', $Repository,
        '--predicate-type', $PredicateType,
        '--signer-workflow', $signerWorkflow,
        '--source-ref', "refs/tags/$ResolvedTag",
        '--source-digest', $SourceDigest,
        '--deny-self-hosted-runners',
        '--limit', '100',
        '--format', 'json'
    )
    if (-not [string]::IsNullOrWhiteSpace($BundlePath)) {
        $arguments += @('--bundle', $BundlePath)
    }

    $results = @(Invoke-GhJson -Arguments $arguments -Description $Description)
    if ($results.Count -eq 0) {
        throw "No matching attestation was verified while: $Description"
    }

    return $results
}

function Assert-SbomPredicatesEqual {
    param(
        [Parameter(Mandatory)]
        [object[]] $VerificationResults,

        [Parameter(Mandatory)]
        [Collections.IDictionary] $ExpectedSbom,

        [Parameter(Mandatory)]
        [string] $Context
    )

    if ($VerificationResults.Count -eq 0) {
        throw "$Context returned no verification results."
    }

    $expectedCanonical = ConvertTo-CanonicalJson -Value $ExpectedSbom
    foreach ($resultValue in $VerificationResults) {
        $result = Get-RequiredDictionary -Value $resultValue -Context "$Context result"
        $verificationResult = Get-RequiredDictionary `
            -Value (Get-RequiredValue `
                -Object $result -Name 'verificationResult' -Context "$Context result") `
            -Context "$Context verificationResult"
        $statement = Get-RequiredDictionary `
            -Value (Get-RequiredValue `
                -Object $verificationResult -Name 'statement' -Context "$Context verificationResult") `
            -Context "$Context statement"
        $predicateType = [string] (Get-RequiredValue `
            -Object $statement -Name 'predicateType' -Context "$Context statement")
        if ($predicateType -cne $cycloneDxPredicateType) {
            throw "$Context returned unexpected predicate type '$predicateType'."
        }
        $predicate = Get-RequiredDictionary `
            -Value (Get-RequiredValue `
                -Object $statement -Name 'predicate' -Context "$Context statement") `
            -Context "$Context predicate"
        $actualCanonical = ConvertTo-CanonicalJson -Value $predicate
        if ($actualCanonical -cne $expectedCanonical) {
            throw "$Context contains an SBOM predicate that differs from the downloaded CycloneDX document."
        }
    }
}

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw 'scripts/verify-release.ps1 requires PowerShell 7 or later.'
}
if (-not (Get-Command gh -CommandType Application -ErrorAction SilentlyContinue)) {
    throw 'GitHub CLI (gh) was not found on PATH.'
}
$null = Invoke-GhCommand `
    -Arguments @('auth', 'status', '--hostname', 'github.com') `
    -Description 'Checking GitHub CLI authentication'

$releaseArguments = @(
    'release', 'view',
    '--repo', $Repository,
    '--json', 'tagName,isDraft,isImmutable,isPrerelease,publishedAt,assets,url'
)
if (-not [string]::IsNullOrWhiteSpace($Tag)) {
    $releaseArguments = @('release', 'view', $Tag) + $releaseArguments[2..($releaseArguments.Count - 1)]
}

try {
    $release = Invoke-GhJson `
        -Arguments $releaseArguments `
        -Description $(if ([string]::IsNullOrWhiteSpace($Tag)) {
            'Resolving the latest published GitHub release'
        }
        else {
            "Resolving GitHub release '$Tag'"
        })
}
catch {
    if ([string]::IsNullOrWhiteSpace($Tag)) {
        throw "No latest published release could be resolved for $Repository.`n$($_.Exception.Message)"
    }
    throw
}

$release = Get-RequiredDictionary -Value $release -Context 'Release'
$resolvedTag = [string] (Get-RequiredValue -Object $release -Name 'tagName' -Context 'Release')
if (-not [string]::IsNullOrWhiteSpace($Tag) -and $resolvedTag -cne $Tag) {
    throw "Requested tag '$Tag' resolved as unexpected tag '$resolvedTag'."
}
$tagMatch = [regex]::Match(
    $resolvedTag,
    '^v(?<version>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)$'
)
if (-not $tagMatch.Success) {
    throw "Release tag '$resolvedTag' is not a supported v-prefixed semantic version."
}
$version = $tagMatch.Groups['version'].Value

if ([bool] (Get-RequiredValue -Object $release -Name 'isDraft' -Context 'Release')) {
    throw "Release '$resolvedTag' is still a draft."
}
if (-not [bool] (Get-RequiredValue -Object $release -Name 'isImmutable' -Context 'Release')) {
    throw "Release '$resolvedTag' is not immutable."
}
$publishedAt = [string] (Get-RequiredValue -Object $release -Name 'publishedAt' -Context 'Release')
if ([string]::IsNullOrWhiteSpace($publishedAt)) {
    throw "Release '$resolvedTag' has no publication timestamp."
}

$crateAssetName = "$crateName-$version.crate"
$sbomAssetName = "$crateName-$version.cdx.json"
$checksumAssetName = 'SHA256SUMS'
$provenanceBundleName = "$crateName-$version.provenance.sigstore.json"
$sbomBundleName = "$crateName-$version.sbom.sigstore.json"
$expectedAssets = @(
    $crateAssetName,
    $sbomAssetName,
    $checksumAssetName,
    $provenanceBundleName,
    $sbomBundleName
)

$assetsValue = Get-RequiredValue -Object $release -Name 'assets' -Context 'Release'
$releaseAssetNames = @($assetsValue | ForEach-Object {
    $asset = Get-RequiredDictionary -Value $_ -Context 'Release asset'
    [string] (Get-RequiredValue -Object $asset -Name 'name' -Context 'Release asset')
})
if ($releaseAssetNames.Count -ne $expectedAssets.Count) {
    throw "Release '$resolvedTag' must contain exactly $($expectedAssets.Count) assets; found $($releaseAssetNames.Count): $($releaseAssetNames -join ', ')"
}
foreach ($expectedAsset in $expectedAssets) {
    if ($releaseAssetNames -cnotcontains $expectedAsset) {
        throw "Release '$resolvedTag' is missing expected asset '$expectedAsset'."
    }
}
foreach ($releaseAsset in $releaseAssetNames) {
    if ($expectedAssets -cnotcontains $releaseAsset) {
        throw "Release '$resolvedTag' contains unexpected asset '$releaseAsset'."
    }
}

if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $downloadDirectory = Join-Path `
        ([IO.Path]::GetTempPath()) `
        ("conpty-oxide-release-$version-" + [Guid]::NewGuid().ToString('N'))
}
else {
    $downloadDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath(
        $OutputDirectory
    )
}
if (Test-Path -LiteralPath $downloadDirectory -PathType Leaf) {
    throw "Output path '$downloadDirectory' is a file, not a directory."
}
New-Item -ItemType Directory -Path $downloadDirectory -Force | Out-Null
$existingEntries = @(Get-ChildItem -LiteralPath $downloadDirectory -Force)
if ($existingEntries.Count -ne 0) {
    throw "Output directory '$downloadDirectory' must be empty."
}
$downloadDirectory = [IO.Path]::GetFullPath(
    (Resolve-Path -LiteralPath $downloadDirectory).ProviderPath
)

$downloadArguments = @(
    'release', 'download', $resolvedTag,
    '--repo', $Repository,
    '--dir', $downloadDirectory
)
foreach ($expectedAsset in $expectedAssets) {
    $downloadArguments += @('--pattern', $expectedAsset)
}
$null = Invoke-GhCommand `
    -Arguments $downloadArguments `
    -Description "Downloading the five assets for release '$resolvedTag'"

$assetPaths = @{}
foreach ($expectedAsset in $expectedAssets) {
    $assetPath = Join-Path $downloadDirectory $expectedAsset
    if (-not (Test-Path -LiteralPath $assetPath -PathType Leaf)) {
        throw "GitHub CLI did not download expected asset '$expectedAsset'."
    }
    $assetPaths[$expectedAsset] = $assetPath
}

$null = Invoke-GhCommand `
    -Arguments @('release', 'verify', $resolvedTag, '--repo', $Repository) `
    -Description "Verifying immutable release attestation for '$resolvedTag'"
foreach ($expectedAsset in $expectedAssets) {
    $null = Invoke-GhCommand `
        -Arguments @(
            'release', 'verify-asset', $resolvedTag, $assetPaths[$expectedAsset],
            '--repo', $Repository
        ) `
        -Description "Verifying release attestation for asset '$expectedAsset'"
}

$checksumTargets = @(
    $crateAssetName,
    $sbomAssetName,
    $provenanceBundleName,
    $sbomBundleName
)
$checksums = [Collections.Generic.Dictionary[string, string]]::new(
    [StringComparer]::Ordinal
)
$checksumLines = [IO.File]::ReadAllLines($assetPaths[$checksumAssetName])
foreach ($line in $checksumLines) {
    if ([string]::IsNullOrWhiteSpace($line)) {
        continue
    }
    $checksumMatch = [regex]::Match(
        $line,
        '^(?<hash>[0-9A-Fa-f]{64}) (?<mode>[ *])(?<name>.+)$'
    )
    if (-not $checksumMatch.Success) {
        throw "SHA256SUMS contains malformed line: '$line'"
    }
    $assetName = $checksumMatch.Groups['name'].Value
    if ($assetName.Contains('/') -or $assetName.Contains('\') -or
        [IO.Path]::GetFileName($assetName) -cne $assetName) {
        throw "SHA256SUMS contains unsafe asset name '$assetName'."
    }
    if ($checksums.ContainsKey($assetName)) {
        throw "SHA256SUMS lists asset '$assetName' more than once."
    }
    $checksums.Add(
        $assetName,
        $checksumMatch.Groups['hash'].Value.ToLowerInvariant()
    )
}
if ($checksums.Count -ne $checksumTargets.Count) {
    throw "SHA256SUMS must contain exactly $($checksumTargets.Count) entries; found $($checksums.Count)."
}
foreach ($checksumTarget in $checksumTargets) {
    if (-not $checksums.ContainsKey($checksumTarget)) {
        throw "SHA256SUMS is missing '$checksumTarget'."
    }
    $actualHash = (Get-FileHash `
        -LiteralPath $assetPaths[$checksumTarget] `
        -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -cne $checksums[$checksumTarget]) {
        throw "SHA-256 mismatch for '$checksumTarget': expected $($checksums[$checksumTarget]), got $actualHash."
    }
}
foreach ($checksumName in $checksums.Keys) {
    if ($checksumTargets -cnotcontains $checksumName) {
        throw "SHA256SUMS contains unexpected entry '$checksumName'."
    }
}
$crateSha256 = $checksums[$crateAssetName]
Write-Information 'SHA256SUMS verified all four non-manifest assets.' -InformationAction Continue

$sparseIndexUri = 'https://index.crates.io/co/np/conpty-oxide'
Write-Information "Checking crates.io sparse index entry for $crateName $version" `
    -InformationAction Continue
try {
    $indexResponse = Invoke-WebRequest `
        -Uri $sparseIndexUri `
        -Headers @{
            Accept = 'text/plain'
            'User-Agent' = 'conpty-oxide-release-verifier/0.1'
        } `
        -TimeoutSec 30
}
catch {
    throw "Failed to download crates.io sparse index entry '$sparseIndexUri'.`n$($_.Exception.Message)"
}
$matchingIndexEntries = @()
foreach ($line in ([string] $indexResponse.Content -split '\r?\n')) {
    if ([string]::IsNullOrWhiteSpace($line)) {
        continue
    }
    try {
        $entry = $line | ConvertFrom-Json -AsHashtable
    }
    catch {
        throw "crates.io sparse index returned malformed JSON.`n$($_.Exception.Message)"
    }
    if ([string] $entry['vers'] -ceq $version) {
        $matchingIndexEntries += $entry
    }
}
if ($matchingIndexEntries.Count -ne 1) {
    throw "Expected one crates.io sparse index entry for $crateName $version; found $($matchingIndexEntries.Count)."
}
$indexChecksum = [string] (Get-RequiredValue `
    -Object $matchingIndexEntries[0] -Name 'cksum' -Context 'crates.io sparse index entry')
if ($indexChecksum.ToLowerInvariant() -cne $crateSha256) {
    throw "GitHub release .crate SHA-256 '$crateSha256' differs from crates.io index checksum '$indexChecksum'."
}
if ($matchingIndexEntries[0].Contains('yanked') -and
    [bool] $matchingIndexEntries[0]['yanked']) {
    Write-Warning "$crateName $version is yanked on crates.io; integrity checks will continue."
}
Write-Information 'The release .crate matches the crates.io sparse index checksum.' `
    -InformationAction Continue

$sbomPath = $assetPaths[$sbomAssetName]
try {
    $sbom = [IO.File]::ReadAllText($sbomPath) | ConvertFrom-Json -AsHashtable
}
catch {
    throw "CycloneDX asset '$sbomAssetName' is not valid JSON.`n$($_.Exception.Message)"
}
$sbom = Get-RequiredDictionary -Value $sbom -Context 'CycloneDX document'
$bomFormat = [string] (Get-RequiredValue `
    -Object $sbom -Name 'bomFormat' -Context 'CycloneDX document')
if ($bomFormat -cne 'CycloneDX') {
    throw "SBOM has unexpected bomFormat '$bomFormat'."
}
$specVersion = [string] (Get-RequiredValue `
    -Object $sbom -Name 'specVersion' -Context 'CycloneDX document')
if ($specVersion -cne '1.5') {
    throw "SBOM has CycloneDX specVersion '$specVersion', expected '1.5'."
}
$metadata = Get-RequiredDictionary `
    -Value (Get-RequiredValue -Object $sbom -Name 'metadata' -Context 'CycloneDX document') `
    -Context 'CycloneDX metadata'
$rootComponent = Get-RequiredDictionary `
    -Value (Get-RequiredValue -Object $metadata -Name 'component' -Context 'CycloneDX metadata') `
    -Context 'CycloneDX root component'
$rootName = [string] (Get-RequiredValue `
    -Object $rootComponent -Name 'name' -Context 'CycloneDX root component')
$rootVersion = [string] (Get-RequiredValue `
    -Object $rootComponent -Name 'version' -Context 'CycloneDX root component')
if ($rootName -cne $crateName) {
    throw "CycloneDX root component name is '$rootName', expected '$crateName'."
}
if ($rootVersion -cne $version) {
    throw "CycloneDX root component version is '$rootVersion', expected '$version'."
}

$rootHashes = @(Get-RequiredValue `
    -Object $rootComponent -Name 'hashes' -Context 'CycloneDX root component')
$rootSha256 = @($rootHashes | Where-Object {
    $_ -is [Collections.IDictionary] -and [string] $_['alg'] -ceq 'SHA-256'
})
if ($rootSha256.Count -ne 1) {
    throw "CycloneDX root component must have exactly one SHA-256 hash; found $($rootSha256.Count)."
}
$rootDistributionHash = [string] (Get-RequiredValue `
    -Object $rootSha256[0] `
    -Name 'content' `
    -Context 'CycloneDX root component SHA-256 hash')
if ($rootDistributionHash.ToLowerInvariant() -cne $crateSha256) {
    throw "CycloneDX root SHA-256 '$rootDistributionHash' differs from .crate SHA-256 '$crateSha256'."
}

$externalReferences = @(Get-RequiredValue `
    -Object $rootComponent `
    -Name 'externalReferences' `
    -Context 'CycloneDX root component')
$distributionReferences = @($externalReferences | Where-Object {
    $_ -is [Collections.IDictionary] -and [string] $_['type'] -ceq 'distribution'
})
if ($distributionReferences.Count -ne 1) {
    throw "CycloneDX root component must have exactly one distribution reference; found $($distributionReferences.Count)."
}
$distribution = Get-RequiredDictionary `
    -Value $distributionReferences[0] `
    -Context 'CycloneDX distribution reference'
$distributionUrl = [string] (Get-RequiredValue `
    -Object $distribution -Name 'url' -Context 'CycloneDX distribution reference')
$expectedDistributionUrl = "https://crates.io/api/v1/crates/$crateName/$version/download"
if ($distributionUrl -cne $expectedDistributionUrl) {
    throw "CycloneDX distribution URL is '$distributionUrl', expected '$expectedDistributionUrl'."
}
$distributionHashes = @(Get-RequiredValue `
    -Object $distribution -Name 'hashes' -Context 'CycloneDX distribution reference')
$distributionSha256 = @($distributionHashes | Where-Object {
    $_ -is [Collections.IDictionary] -and [string] $_['alg'] -ceq 'SHA-256'
})
if ($distributionSha256.Count -ne 1) {
    throw "CycloneDX distribution reference must have exactly one SHA-256 hash; found $($distributionSha256.Count)."
}
$sbomDistributionHash = [string] (Get-RequiredValue `
    -Object $distributionSha256[0] `
    -Name 'content' `
    -Context 'CycloneDX distribution SHA-256 hash')
if ($sbomDistributionHash.ToLowerInvariant() -cne $crateSha256) {
    throw "CycloneDX distribution SHA-256 '$sbomDistributionHash' differs from .crate SHA-256 '$crateSha256'."
}
Write-Information 'CycloneDX root component and crates.io distribution metadata passed.' `
    -InformationAction Continue

$sourceDigest = Resolve-TagCommit -ResolvedTag $resolvedTag
$provenanceBundlePath = $assetPaths[$provenanceBundleName]
$sbomBundlePath = $assetPaths[$sbomBundleName]

$null = @(Invoke-AttestationVerification `
    -ArtifactPath $assetPaths[$crateAssetName] `
    -PredicateType $provenancePredicateType `
    -ResolvedTag $resolvedTag `
    -SourceDigest $sourceDigest `
    -Description 'Verifying online SLSA provenance for the .crate asset')
$null = @(Invoke-AttestationVerification `
    -ArtifactPath $assetPaths[$crateAssetName] `
    -PredicateType $provenancePredicateType `
    -ResolvedTag $resolvedTag `
    -SourceDigest $sourceDigest `
    -BundlePath $provenanceBundlePath `
    -Description 'Verifying bundled SLSA provenance for the .crate asset')
$null = @(Invoke-AttestationVerification `
    -ArtifactPath $sbomPath `
    -PredicateType $provenancePredicateType `
    -ResolvedTag $resolvedTag `
    -SourceDigest $sourceDigest `
    -Description 'Verifying online SLSA provenance for the CycloneDX asset')
$null = @(Invoke-AttestationVerification `
    -ArtifactPath $sbomPath `
    -PredicateType $provenancePredicateType `
    -ResolvedTag $resolvedTag `
    -SourceDigest $sourceDigest `
    -BundlePath $provenanceBundlePath `
    -Description 'Verifying bundled SLSA provenance for the CycloneDX asset')

$onlineSbomResults = @(Invoke-AttestationVerification `
    -ArtifactPath $assetPaths[$crateAssetName] `
    -PredicateType $cycloneDxPredicateType `
    -ResolvedTag $resolvedTag `
    -SourceDigest $sourceDigest `
    -Description 'Verifying the online CycloneDX attestation for the .crate asset')
Assert-SbomPredicatesEqual `
    -VerificationResults $onlineSbomResults `
    -ExpectedSbom $sbom `
    -Context 'Online CycloneDX attestation'

$bundledSbomResults = @(Invoke-AttestationVerification `
    -ArtifactPath $assetPaths[$crateAssetName] `
    -PredicateType $cycloneDxPredicateType `
    -ResolvedTag $resolvedTag `
    -SourceDigest $sourceDigest `
    -BundlePath $sbomBundlePath `
    -Description 'Verifying the bundled CycloneDX attestation for the .crate asset')
Assert-SbomPredicatesEqual `
    -VerificationResults $bundledSbomResults `
    -ExpectedSbom $sbom `
    -Context 'Bundled CycloneDX attestation'

Write-Information "Release '$resolvedTag' passed all integrity and provenance checks." `
    -InformationAction Continue
Write-Output $downloadDirectory
