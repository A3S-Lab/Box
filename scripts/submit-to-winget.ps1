# Submit a3s-box to Windows Package Manager (WinGet).
#
# Usage:
#   .\scripts\submit-to-winget.ps1 -Version "<VERSION>"
#   .\scripts\submit-to-winget.ps1 -Version "<VERSION>" -ValidateOnly
#
# Version must be strict SemVer 2.0.0 without a leading "v". The script
# downloads the pinned WinGetCreate executable and verifies its SHA256 before
# using it. Submission requires -GitHubToken or GITHUB_TOKEN.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string] $Version,

    [Parameter(Mandatory = $false)]
    [string] $GitHubToken = $env:GITHUB_TOKEN,

    [Parameter(Mandatory = $false)]
    [switch] $ValidateOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$WingetCreateVersion = '1.12.8.0'
$WingetCreateUrl = 'https://github.com/microsoft/winget-create/releases/download/v1.12.8.0/wingetcreate.exe'
$WingetCreateSha256 = '8BD738851B524885410112678E3771B341C5C716DE60FBBECB88AB0A363ED85D'
$Repository = 'A3S-Lab/Box'
$SemVerPattern = '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z'
$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)

function Set-SingleManifestValue {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $Key,
        [Parameter(Mandatory = $true)] [string] $Value
    )

    $content = [System.IO.File]::ReadAllText($Path)
    $pattern = "(?m)^(?<indent>[ `t]*)$([regex]::Escape($Key)):[^`r`n]*(?=`r?$)"
    $matches = [regex]::Matches($content, $pattern)
    if ($matches.Count -ne 1) {
        throw "Expected exactly one '$Key' entry in $Path; found $($matches.Count)."
    }

    $replacement = '${indent}' + "${Key}: $Value"
    $updated = [regex]::Replace($content, $pattern, $replacement)
    [System.IO.File]::WriteAllText($Path, $updated, $script:Utf8NoBom)
}

function Set-NestedInstallerVersion {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $Tag
    )

    $content = [System.IO.File]::ReadAllText($Path)
    $pattern = '(?m)^(?<prefix>[ \t]*-[ \t]*RelativeFilePath:[ \t]*)a3s-box-v[^\r\n\\]+-windows-x86_64(?=\\)'
    $matches = [regex]::Matches($content, $pattern)
    if ($matches.Count -ne 2) {
        throw "Expected exactly two nested installer paths in $Path; found $($matches.Count)."
    }

    $replacement = '${prefix}' + "a3s-box-$Tag-windows-x86_64"
    $updated = [regex]::Replace($content, $pattern, $replacement)
    [System.IO.File]::WriteAllText($Path, $updated, $script:Utf8NoBom)
}

if ($Version -cnotmatch $SemVerPattern) {
    throw "Version '$Version' is not strict SemVer 2.0.0 without a leading v."
}

if (-not $ValidateOnly -and [string]::IsNullOrWhiteSpace($GitHubToken)) {
    throw 'GITHUB_TOKEN is not set. Pass -GitHubToken, set GITHUB_TOKEN, or use -ValidateOnly.'
}

$Tag = "v$Version"
$AssetName = "a3s-box-$Tag-windows-x86_64.zip"
$AssetUrl = "https://github.com/$Repository/releases/download/$Tag/$AssetName"
$ManifestDir = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\.winget'))
$VersionManifest = Join-Path $ManifestDir 'A3SLab.Box.yaml'
$InstallerManifest = Join-Path $ManifestDir 'A3SLab.Box.installer.yaml'
$LocaleManifest = Join-Path $ManifestDir 'A3SLab.Box.locale.en-US.yaml'

foreach ($manifest in @($VersionManifest, $InstallerManifest, $LocaleManifest)) {
    if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
        throw "Required manifest not found: $manifest"
    }
}

$WingetCreatePath = Join-Path ([System.IO.Path]::GetTempPath()) "a3s-box-wingetcreate-$WingetCreateVersion-$PID.exe"
$AssetPath = Join-Path ([System.IO.Path]::GetTempPath()) "a3s-box-$Tag-windows-x86_64-$PID.zip"

try {
    Write-Host "Downloading WinGetCreate $WingetCreateVersion..." -ForegroundColor Yellow
    Invoke-WebRequest -Uri $WingetCreateUrl -OutFile $WingetCreatePath
    $actualToolSha256 = (Get-FileHash -LiteralPath $WingetCreatePath -Algorithm SHA256).Hash
    if (-not [string]::Equals(
        $actualToolSha256,
        $WingetCreateSha256,
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw "WinGetCreate SHA256 mismatch: expected $WingetCreateSha256, got $actualToolSha256."
    }
    Write-Host "Verified WinGetCreate SHA256: $actualToolSha256" -ForegroundColor Green
    & $WingetCreatePath info
    if ($LASTEXITCODE -ne 0) {
        throw "The verified WinGetCreate executable failed with exit code $LASTEXITCODE."
    }

    Write-Host "Downloading release asset: $AssetUrl" -ForegroundColor Yellow
    Invoke-WebRequest -Uri $AssetUrl -OutFile $AssetPath
    $assetSha256 = (Get-FileHash -LiteralPath $AssetPath -Algorithm SHA256).Hash
    Write-Host "Release asset SHA256: $assetSha256" -ForegroundColor Green

    Write-Host 'Updating manifest files...' -ForegroundColor Yellow
    foreach ($manifest in @($VersionManifest, $InstallerManifest, $LocaleManifest)) {
        Set-SingleManifestValue -Path $manifest -Key 'PackageVersion' -Value $Version
    }

    Set-SingleManifestValue -Path $InstallerManifest -Key 'InstallerUrl' -Value $AssetUrl
    Set-SingleManifestValue -Path $InstallerManifest -Key 'InstallerSha256' -Value $assetSha256
    Set-NestedInstallerVersion -Path $InstallerManifest -Tag $Tag

    Set-SingleManifestValue -Path $LocaleManifest -Key 'PublisherUrl' -Value 'https://github.com/A3S-Lab'
    Set-SingleManifestValue -Path $LocaleManifest -Key 'PublisherSupportUrl' -Value "https://github.com/$Repository/issues"
    Set-SingleManifestValue -Path $LocaleManifest -Key 'PackageUrl' -Value "https://github.com/$Repository"
    Set-SingleManifestValue -Path $LocaleManifest -Key 'LicenseUrl' -Value "https://github.com/$Repository/blob/main/LICENSE"
    Set-SingleManifestValue -Path $LocaleManifest -Key 'ReleaseNotesUrl' -Value "https://github.com/$Repository/releases/tag/$Tag"

    $winget = Get-Command winget -CommandType Application -ErrorAction SilentlyContinue
    if (-not $winget) {
        throw 'WinGet is unavailable; install or repair Windows Package Manager before validating manifests.'
    }

    Write-Host 'Validating manifests with WinGet...' -ForegroundColor Yellow
    & $winget.Source validate --manifest $ManifestDir --disable-interactivity
    if ($LASTEXITCODE -ne 0) {
        throw "WinGet manifest validation failed with exit code $LASTEXITCODE."
    }
    Write-Host 'Manifests validated successfully.' -ForegroundColor Green

    if ($ValidateOnly) {
        Write-Host 'Validation-only mode: no pull request was submitted.' -ForegroundColor Cyan
        return
    }

    Write-Host 'Submitting manifests to microsoft/winget-pkgs...' -ForegroundColor Yellow
    & $WingetCreatePath submit --no-open --token $GitHubToken $ManifestDir
    if ($LASTEXITCODE -ne 0) {
        throw "WinGetCreate submission failed with exit code $LASTEXITCODE. The validated manifests remain in $ManifestDir for manual submission."
    }

    Write-Host 'WinGetCreate submitted the manifests successfully.' -ForegroundColor Green
    Write-Host 'Monitor the pull request at https://github.com/microsoft/winget-pkgs/pulls' -ForegroundColor Cyan
}
finally {
    foreach ($temporaryFile in @($WingetCreatePath, $AssetPath)) {
        if (Test-Path -LiteralPath $temporaryFile -PathType Leaf) {
            Remove-Item -LiteralPath $temporaryFile -Force
        }
    }
}
