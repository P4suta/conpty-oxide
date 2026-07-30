# SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
# SPDX-License-Identifier: MIT OR Apache-2.0

<#
.SYNOPSIS
Runs ShellCheck over every explicit Bash block in one GitHub Actions workflow.

.DESCRIPTION
Actionlint's Windows pipe to ShellCheck can deadlock once a workflow contains
enough large embedded scripts. Actionlint still validates the complete YAML
and expression surface; this helper feeds each literal Bash block to
ShellCheck separately. ExpectedCount makes the indentation parser fail closed
when the workflow gains, loses, or changes a Bash block.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Path,

    [ValidateSet('bash', 'none')]
    [string] $DefaultShell = 'none',

    [Parameter(Mandatory)]
    [ValidateRange(1, 1000)]
    [int] $ExpectedCount
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$lines = @(Get-Content -LiteralPath $Path)
$blocks = [System.Collections.Generic.List[object]]::new()

for ($lineIndex = 0; $lineIndex -lt $lines.Count; $lineIndex++) {
    $runMatch = [regex]::Match(
        $lines[$lineIndex],
        '^(?<indent>\s*)run:\s*(?<style>[|>])[-+]?\s*$'
    )
    if (-not $runMatch.Success) {
        continue
    }

    $runIndent = $runMatch.Groups['indent'].Value.Length
    $stepShell = $DefaultShell
    $stepName = "line $($lineIndex + 1)"
    for ($previous = $lineIndex - 1; $previous -ge 0; $previous--) {
        if ([string]::IsNullOrWhiteSpace($lines[$previous])) {
            continue
        }

        $previousIndent = [regex]::Match($lines[$previous], '^\s*').Value.Length
        if ($previousIndent -lt $runIndent) {
            $nameMatch = [regex]::Match($lines[$previous], '^\s*-\s+name:\s*(?<name>.+?)\s*$')
            if ($nameMatch.Success) {
                $stepName = $nameMatch.Groups['name'].Value
            }
            break
        }
        if ($previousIndent -eq $runIndent) {
            $shellMatch = [regex]::Match(
                $lines[$previous],
                '^\s*shell:\s*["'']?(?<shell>[^"''\s]+)["'']?\s*$'
            )
            if ($shellMatch.Success) {
                $stepShell = $shellMatch.Groups['shell'].Value
            }
        }
    }

    if ($stepShell -ne 'bash') {
        continue
    }
    if ($runMatch.Groups['style'].Value -ne '|') {
        throw "Explicit Bash step '$stepName' must use a literal run block, not a folded block."
    }

    $endIndex = $lineIndex + 1
    $blockIndent = $null
    while ($endIndex -lt $lines.Count) {
        $candidate = $lines[$endIndex]
        if (-not [string]::IsNullOrWhiteSpace($candidate)) {
            $candidateIndent = [regex]::Match($candidate, '^\s*').Value.Length
            if ($candidateIndent -le $runIndent) {
                break
            }
            if ($null -eq $blockIndent) {
                $blockIndent = $candidateIndent
            }
        }
        $endIndex++
    }
    if ($null -eq $blockIndent) {
        throw "Explicit Bash step '$stepName' has an empty run block."
    }

    $content = [System.Collections.Generic.List[string]]::new()
    for ($contentIndex = $lineIndex + 1; $contentIndex -lt $endIndex; $contentIndex++) {
        $contentLine = $lines[$contentIndex]
        if ([string]::IsNullOrWhiteSpace($contentLine)) {
            $content.Add('')
            continue
        }
        if ($contentLine.Length -lt $blockIndent) {
            throw "Malformed indentation in Bash step '$stepName' at line $($contentIndex + 1)."
        }
        $content.Add($contentLine.Substring($blockIndent))
    }

    $blocks.Add([pscustomobject]@{
            Name    = $stepName
            Line    = $lineIndex + 1
            Content = $content
        })
    $lineIndex = $endIndex - 1
}

if ($blocks.Count -ne $ExpectedCount) {
    throw "Found $($blocks.Count) explicit Bash blocks in '$Path'; expected $ExpectedCount."
}

$failed = $false
foreach ($block in $blocks) {
    $temporary = [System.IO.Path]::GetTempFileName()
    try {
        $script = ($block.Content -join "`n") + "`n"
        [System.IO.File]::WriteAllText(
            $temporary,
            $script,
            [System.Text.UTF8Encoding]::new($false)
        )
        $diagnostics = @(& shellcheck --shell bash --format gcc $temporary 2>&1)
        $exitCode = $LASTEXITCODE
        if ($exitCode -ne 0) {
            $failed = $true
            Write-Output "ShellCheck failed for '$($block.Name)' at ${Path}:$($block.Line):"
            $diagnostics | Write-Output
        }
    }
    finally {
        Remove-Item -LiteralPath $temporary -Force
    }
}

if ($failed) {
    throw "ShellCheck rejected one or more Bash blocks in '$Path'."
}

Write-Output "ShellCheck passed for all $($blocks.Count) explicit Bash blocks in '$Path'."
