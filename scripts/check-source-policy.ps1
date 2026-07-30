# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
# SPDX-License-Identifier: MIT OR Apache-2.0

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$repositoryPrefix = $repositoryRoot.TrimEnd("\", "/") + [IO.Path]::DirectorySeparatorChar
$excludedDirectories = @(".git", "target", "vendor")
$violations = [Collections.Generic.List[string]]::new()

function Get-RelativeRepositoryPath {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $fullPath = [IO.Path]::GetFullPath($Path)
    if (-not $fullPath.StartsWith($repositoryPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Path is outside the repository root: $fullPath"
    }

    return $fullPath.Substring($repositoryPrefix.Length).Replace("\", "/")
}

function Get-LineNumber {
    param(
        [Parameter(Mandatory)]
        [string] $Content,

        [Parameter(Mandatory)]
        [int] $Index
    )

    if ($Index -eq 0) {
        return 1
    }

    return 1 + [regex]::Matches($Content.Substring(0, $Index), "\n").Count
}

function Add-PolicyMatch {
    param(
        [Parameter(Mandatory)]
        [string] $RelativePath,

        [Parameter(Mandatory)]
        [string] $Content,

        [Parameter(Mandatory)]
        [regex] $Pattern,

        [Parameter(Mandatory)]
        [string] $Rule,

        [scriptblock] $IsAllowed
    )

    foreach ($match in $Pattern.Matches($Content)) {
        if ($null -ne $IsAllowed -and (& $IsAllowed $RelativePath $match.Value)) {
            continue
        }

        $line = Get-LineNumber -Content $Content -Index $match.Index
        $display = ($match.Value -replace "\s+", " ").Trim()
        $violations.Add("${RelativePath}:${line}: ${Rule}: ${display}")
    }
}

$rustFiles = Get-ChildItem -LiteralPath $repositoryRoot -Recurse -File -Filter "*.rs" |
    Where-Object {
        $relativePath = Get-RelativeRepositoryPath -Path $_.FullName
        $firstComponent = ($relativePath -split "/", 2)[0]
        $excludedDirectories -notcontains $firstComponent
    }

# `std::error::Error::source` requires this exact trait object in its method
# signature. Every other project-owned dynamic-dispatch site remains banned.
$dynamicDispatch = [regex]::new(
    "\bdyn\b(?!\s+error::Error\b)",
    [Text.RegularExpressions.RegexOptions]::CultureInvariant
)
$ignoredTest = [regex]::new(
    "#\s*!?\s*\[\s*ignore(?:\s*(?:\([^]]*\)|=[^]]]*))?\s*\]",
    [Text.RegularExpressions.RegexOptions]::CultureInvariant
)
$lintSuppression = [regex]::new(
    "#\s*!?\s*\[\s*(?:(?:cfg_attr)\s*\([^]]*?)?\b(?:allow|expect)\s*\([^)]*\)[^]]*\]",
    [Text.RegularExpressions.RegexOptions]::CultureInvariant -bor
        [Text.RegularExpressions.RegexOptions]::Singleline
)

foreach ($file in $rustFiles) {
    $relativePath = Get-RelativeRepositoryPath -Path $file.FullName
    $content = [IO.File]::ReadAllText($file.FullName)

    Add-PolicyMatch -RelativePath $relativePath -Content $content -Pattern $dynamicDispatch `
        -Rule "dynamic dispatch (`dyn`) is forbidden in project-owned Rust"
    Add-PolicyMatch -RelativePath $relativePath -Content $content -Pattern $ignoredTest `
        -Rule "ignored tests are forbidden"
    Add-PolicyMatch -RelativePath $relativePath -Content $content -Pattern $lintSuppression `
        -Rule "lint allow/expect attributes are forbidden"
}

if ($violations.Count -ne 0) {
    foreach ($violation in ($violations | Sort-Object)) {
        [Console]::Error.WriteLine($violation)
    }

    exit 1
}

Write-Output "Source policy passed for $($rustFiles.Count) Rust files."
